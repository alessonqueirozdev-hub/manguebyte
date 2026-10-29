//! A harness for testing interfaces with no window, no host and no
//! screen.
//!
//! A [`Harness`] owns a [`Root`] and a
//! [`RecordingEditorHost`]
//! that records every gesture the interface sends. It runs frames on a
//! clock the test moves, sends pointer, wheel and key input, and shows the
//! result as pixels, as a scene dump, as damage and as the calls the host
//! received.
//!
//! Widgets are found only by [`Role`] and name, as a screen reader finds
//! them: a control that assistive technology cannot find cannot be tested
//! either.
//!
//! ```
//! use std::sync::Arc;
//! use manguebyte_gfx::Size;
//! use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
//! use manguebyte_plugin::editor::testing::HostCall;
//! use manguebyte_ui::testing::Harness;
//! use manguebyte_ui::{knob, ParamHandle, Role};
//!
//! struct Params { depth: FloatParam }
//! param_set!(Params { depth });
//! let params = Arc::new(Params {
//!     depth: FloatParam::new("depth", "Depth", FloatRange::linear(0.0, 100.0), 50.0)
//!         .with_unit("%")
//!         .with_precision(0),
//! });
//!
//! let view = knob(ParamHandle::new(Arc::clone(&params), |p| &p.depth));
//! let mut ui = Harness::with_params(view, Size::new(80.0, 100.0), 2.0, params.clone());
//! ui.frame();
//!
//! let depth = ui.get(Role::Slider, "Depth");
//! assert_eq!(depth.value_text(), Some("50 %"));
//! ui.drag(depth.center(), (0.0, -40.0), 4);
//! ui.frame();
//!
//! // One gesture: a begin, four values, an end; 40 pixels up is a fifth
//! // of the theme's 200-pixel drag range.
//! let id = params.depth.param_id();
//! let calls = ui.take_calls();
//! assert_eq!(calls.first(), Some(&HostCall::BeginGesture(id)));
//! assert_eq!(calls.last(), Some(&HostCall::EndGesture(id)));
//! assert_eq!(calls.len(), 6);
//! assert_eq!(params.depth.value(), 70.0);
//! assert_eq!(ui.get(Role::Slider, "Depth").value_text(), Some("70 %"));
//! ```

use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use manguebyte_gfx::{IRect, Pixmap, Point, Rect, Renderer, Size};
use manguebyte_params::ParamSet;
use manguebyte_plugin::editor::testing::{HostCall, RecordingEditorHost};
use manguebyte_plugin::editor::{EditorContext, HostLink};

use crate::access::{AccessEntry, AccessNode, Action, Role, first_problem};
use crate::clipboard::MemoryClipboard;
use crate::event::{Button, Event, Key, KeyEvent, Modifiers};
use crate::root::Root;
use crate::theme::Theme;
use crate::tree::NodeId;
use crate::view::IntoView;

/// A widget the harness found, as it was when it was found.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    /// The widget, for [`Root`] methods that take one.
    pub id: NodeId,
    /// For an item of a widget, such as a row of a list, its key (see
    /// [`AccessItem`](crate::AccessItem)); `None` for a widget.
    pub item: Option<usize>,
    /// How it describes itself.
    pub node: AccessNode,
    /// Where it is, in logical pixels.
    pub bounds: Rect,
    /// Where it is, in device pixels.
    pub device: IRect,
}

impl Found {
    fn from_entry(entry: AccessEntry) -> Found {
        Found {
            id: entry.id,
            item: entry.item,
            node: entry.node,
            bounds: entry.bounds,
            device: entry.device,
        }
    }

    /// The centre of the widget, where a click on it goes.
    pub fn center(&self) -> Point {
        self.bounds.center()
    }

    /// Its role.
    pub fn role(&self) -> Role {
        self.node.role
    }

    /// Its name.
    pub fn name(&self) -> &str {
        &self.node.name
    }

    /// Its value as text, if it has one.
    pub fn value_text(&self) -> Option<&str> {
        self.node.value.as_deref()
    }

    /// Its value as a number, if it has one.
    pub fn numeric(&self) -> Option<f64> {
        self.node.numeric
    }

    /// Whether it had the keyboard focus.
    pub fn is_focused(&self) -> bool {
        self.node.state.focused
    }
}

/// What one frame run by the harness did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSummary {
    /// How many damage rectangles it drew (see
    /// [`Harness::last_damage`]).
    pub damaged: usize,
    /// How many widgets recorded a scene that differs from the one they
    /// had.
    pub painted: usize,
    /// Whether layout ran.
    pub laid_out: bool,
    /// Whether something wanted another frame.
    pub animating: bool,
}

/// An interface under test: a [`Root`], a recording host and a clock.
pub struct Harness {
    // Dropped in this order: the interface first, which ends its open
    // gestures, then the link to the host.
    root: Root,
    link: Option<HostLink>,
    host: Rc<RecordingEditorHost>,
    clipboard: Rc<MemoryClipboard>,
    clock: Duration,
    period: Duration,
    damage: Vec<IRect>,
    frames: u64,
    drawn: u64,
    modifiers: Modifiers,
    pointer: Point,
}

impl Harness {
    /// `view` at `size` logical pixels and `scale`, with a host that
    /// records gestures and stores nothing, as a host that echoes edits
    /// late or never: parameters keep their values unless a test sets
    /// them, and controls show a value they sent while a gesture is open
    /// and for [`ParamBinding::ECHO_WAIT`](crate::ParamBinding::ECHO_WAIT)
    /// after it, on the harness's clock.
    pub fn new(view: impl IntoView, size: Size, scale: f32) -> Harness {
        Harness::with_host(view, size, scale, Rc::new(RecordingEditorHost::new()))
    }

    /// `view` at `size` and `scale`, with a host that records gestures and
    /// stores every value sent into `params`, as a standalone program
    /// does, so that controls read back what they set.
    pub fn with_params(
        view: impl IntoView,
        size: Size,
        scale: f32,
        params: Arc<dyn ParamSet>,
    ) -> Harness {
        let host = Rc::new(RecordingEditorHost::new().with_params(params));
        Harness::with_host(view, size, scale, host)
    }

    /// `view` at `size` and `scale`, with `host`.
    pub fn with_host(
        view: impl IntoView,
        size: Size,
        scale: f32,
        host: Rc<RecordingEditorHost>,
    ) -> Harness {
        let (context, link) = EditorContext::attach(host.clone());
        let clipboard = Rc::new(MemoryClipboard::new());
        Harness {
            root: Root::new(view, size, scale, context).with_clipboard(clipboard.clone()),
            link: Some(link),
            host,
            clipboard,
            clock: Duration::ZERO,
            period: Duration::from_millis(16),
            damage: Vec::new(),
            frames: 0,
            drawn: 0,
            modifiers: Modifiers::NONE,
            pointer: Point::ZERO,
        }
    }

    /// Draws the interface in `theme` from the next frame on.
    pub fn set_theme(&mut self, theme: Theme) {
        self.root.set_theme(theme);
    }

    /// The interface.
    pub fn root(&self) -> &Root {
        &self.root
    }

    /// The interface, to drive it directly.
    pub fn root_mut(&mut self) -> &mut Root {
        &mut self.root
    }

    /// The host.
    pub fn host(&self) -> &RecordingEditorHost {
        &self.host
    }

    /// Every call the interface made on the host so far.
    pub fn calls(&self) -> Vec<HostCall> {
        self.host.calls()
    }

    /// Every call the interface made on the host so far, forgetting them.
    pub fn take_calls(&self) -> Vec<HostCall> {
        self.host.take_calls()
    }

    /// The clipboard the interface copies to and pastes from: one in
    /// memory, which the test reads and fills.
    pub fn clipboard(&self) -> &MemoryClipboard {
        &self.clipboard
    }

    /// Closes the interface as a wrapper does: drops it, which ends any
    /// gesture still open, then the link to the host. Returns the calls
    /// the host received, including those made while closing.
    pub fn close(self) -> Vec<HostCall> {
        let Harness {
            root, link, host, ..
        } = self;
        drop(root);
        drop(link);
        host.take_calls()
    }

    // The clock.

    /// The time on the harness's clock.
    pub fn now(&self) -> Duration {
        self.clock
    }

    /// How far [`advance`](Harness::advance) moves the clock between
    /// frames: 16 ms unless set.
    pub fn set_period(&mut self, period: Duration) {
        self.period = period.max(Duration::from_micros(1));
    }

    /// Runs one frame at the clock's time, without moving the clock.
    pub fn frame(&mut self) -> FrameSummary {
        let frame = self.root.frame(self.clock);
        self.damage.clear();
        self.damage.extend_from_slice(frame.damage);
        let summary = FrameSummary {
            damaged: frame.damage.len(),
            painted: frame.painted,
            laid_out: frame.laid_out,
            animating: frame.animating,
        };
        self.frames += 1;
        if summary.damaged > 0 {
            self.drawn += 1;
        }
        summary
    }

    /// Moves the clock on by `by`, running a frame every period on the
    /// way and one at the end; none if `by` is zero.
    pub fn advance(&mut self, by: Duration) {
        let end = self.clock.saturating_add(by);
        while self.clock < end {
            self.clock = self.clock.saturating_add(self.period).min(end);
            self.frame();
        }
    }

    /// [`advance`](Harness::advance) by `ms` milliseconds.
    pub fn advance_ms(&mut self, ms: u64) {
        self.advance(Duration::from_millis(ms));
    }

    /// How many frames have run.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// How many frames drew something.
    pub fn frames_rendered(&self) -> u64 {
        self.drawn
    }

    /// How many frames drew something while `work` ran.
    pub fn frames_rendered_during(&mut self, work: impl FnOnce(&mut Harness)) -> u64 {
        let before = self.drawn;
        work(self);
        self.drawn - before
    }

    /// The damage the last frame drew, in device pixels.
    pub fn last_damage(&self) -> &[IRect] {
        &self.damage
    }

    /// The rectangle holding all of the last frame's damage, if there was
    /// any.
    pub fn last_damage_bounds(&self) -> Option<IRect> {
        self.damage
            .iter()
            .copied()
            .reduce(|all, rect| all.union(&rect))
    }

    // Finding widgets.

    /// Every widget, in tree order.
    pub fn everything(&self) -> Vec<Found> {
        self.root
            .access_tree()
            .into_iter()
            .map(Found::from_entry)
            .collect()
    }

    /// Every widget with `role` and `name`.
    pub fn find_all(&self, role: Role, name: &str) -> Vec<Found> {
        self.everything()
            .into_iter()
            .filter(|found| found.node.role == role && found.node.name == name)
            .collect()
    }

    /// The widget with `role` and `name`, if there is exactly one.
    pub fn find(&self, role: Role, name: &str) -> Option<Found> {
        let mut all = self.find_all(role, name);
        if all.len() == 1 { all.pop() } else { None }
    }

    /// The widget with `role` and `name`.
    ///
    /// # Panics
    ///
    /// If there is none, or more than one, listing what there is.
    pub fn get(&self, role: Role, name: &str) -> Found {
        let mut all = self.find_all(role, name);
        match all.len() {
            1 => all.remove(0),
            0 => panic!(
                "no {role:?} named {name:?}; the interface has: {}",
                self.describe()
            ),
            count => {
                panic!("{count} widgets are {role:?} named {name:?}; names must tell them apart")
            }
        }
    }

    /// The widget with the keyboard focus, if any.
    pub fn focused(&self) -> Option<Found> {
        let id = self.root.focus()?;
        self.root.access(id).map(Found::from_entry)
    }

    /// Every widget as `Role "name" = value`, one per line.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for found in self.everything() {
            out.push_str(&format!("\n  {:?} {:?}", found.node.role, found.node.name));
            if let Some(value) = &found.node.value {
                out.push_str(&format!(" = {value:?}"));
            }
        }
        out
    }

    /// Checks the accessibility tree and the theme:
    ///
    /// - every widget that takes the focus has a name;
    /// - every widget that can be pressed or adjusted takes the focus, so
    ///   that Tab reaches it;
    /// - every slider has its value as text and as a number within a
    ///   range, and accepts increment, decrement and set value;
    /// - every meter has its value as text, and every switch says whether
    ///   it is on;
    /// - every text input has its text as its value and takes the focus;
    /// - every combo box has its value as text, can be pressed to open,
    ///   and says whether it is open;
    /// - every list and scroll view has a name, and every list item says
    ///   whether it is selected;
    /// - no two widgets with a name share their role and name within the
    ///   same named group, so a screen reader, and
    ///   [`get`](Harness::get), can tell them apart;
    /// - the theme's text and muted text colours have a contrast of at
    ///   least [`MIN_TEXT_CONTRAST`](crate::access::MIN_TEXT_CONTRAST) on
    ///   its background and surface colours, and its text colour on its
    ///   track colour (a menu's highlighted item) and on the selection of
    ///   a text input.
    ///
    /// # Panics
    ///
    /// On the first rule broken, saying which and where.
    pub fn assert_accessible(&self) {
        if let Some(problem) = first_problem(&self.root.access_tree(), self.root.theme()) {
            panic!("{problem}");
        }
    }

    /// Asks the widget for `action`, as assistive technology does.
    pub fn act(&mut self, found: &Found, action: Action) -> bool {
        match found.item {
            Some(key) => self.root.act_item(found.id, key, action),
            None => self.root.act(found.id, action),
        }
    }

    // Input.

    /// Holds `modifiers` for the pointer events that follow.
    pub fn set_modifiers(&mut self, modifiers: Modifiers) {
        self.modifiers = modifiers;
    }

    /// Sends `event`.
    pub fn send(&mut self, event: Event) -> bool {
        if let Some(at) = event.position() {
            self.pointer = at;
        }
        self.root.event(event).is_yes()
    }

    /// Presses the main button at `at`.
    pub fn press(&mut self, at: Point) -> bool {
        self.press_with(at, 1)
    }

    fn press_with(&mut self, at: Point, clicks: u32) -> bool {
        self.send(Event::PointerDown {
            at,
            button: Button::Primary,
            clicks,
            modifiers: self.modifiers,
        })
    }

    /// Moves the pointer to `at`.
    pub fn move_to(&mut self, at: Point) -> bool {
        self.send(Event::PointerMove {
            at,
            modifiers: self.modifiers,
        })
    }

    /// Releases the main button at `at`.
    pub fn release(&mut self, at: Point) -> bool {
        self.send(Event::PointerUp {
            at,
            button: Button::Primary,
            modifiers: self.modifiers,
        })
    }

    /// Presses and releases the main button at `at`.
    pub fn click(&mut self, at: Point) {
        self.press(at);
        self.release(at);
    }

    /// Two clicks at `at`, the second counted as a double click.
    pub fn double_click(&mut self, at: Point) {
        self.click(at);
        self.press_with(at, 2);
        self.release(at);
    }

    /// Presses at `from`, moves by `by` in `steps` equal moves, and
    /// releases. No frame runs in between.
    pub fn drag(&mut self, from: Point, by: (f32, f32), steps: u32) {
        self.press(from);
        let steps = steps.max(1);
        let mut at = from;
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            at = Point::new(from.x + by.0 * t, from.y + by.1 * t);
            self.move_to(at);
        }
        self.release(at);
    }

    /// Turns the wheel by `notches` (above 0: away from the person) with
    /// the pointer at `at`.
    pub fn wheel(&mut self, at: Point, notches: f32) -> bool {
        self.send(Event::Wheel {
            at,
            delta: Point::new(0.0, notches),
            modifiers: self.modifiers,
        })
    }

    /// Scrolls by `delta` notches with the pointer at `at`: fractions of a
    /// notch, as a touchpad gives, and sideways (`x` above 0 is to the
    /// right) as well as up and down.
    pub fn wheel_by(&mut self, at: Point, delta: Point) -> bool {
        self.send(Event::Wheel {
            at,
            delta,
            modifiers: self.modifiers,
        })
    }

    /// Presses and releases `key`.
    pub fn key(&mut self, key: Key) -> bool {
        self.key_with(key, Modifiers::NONE)
    }

    /// Presses and releases `key` with `modifiers` held. Returns whether
    /// the press was used.
    pub fn key_with(&mut self, key: Key, modifiers: Modifiers) -> bool {
        let used = self.send(Event::Key(KeyEvent {
            key,
            modifiers,
            pressed: true,
        }));
        self.send(Event::Key(KeyEvent {
            key,
            modifiers,
            pressed: false,
        }));
        used
    }

    /// Types `text` as a keyboard does, character by character: for each,
    /// the key goes down, the character is typed, and the key comes up.
    /// Returns whether every character was used. Line breaks and tabs are
    /// typed as the keys Enter and Tab, which type no text.
    pub fn type_text(&mut self, text: &str) -> bool {
        let mut used = true;
        for c in text.chars() {
            let key = match c {
                '\n' | '\r' => Key::Enter,
                '\t' => Key::Tab,
                c => Key::Character(c.to_lowercase().next().unwrap_or(c)),
            };
            let shift = c.is_uppercase();
            let modifiers = Modifiers {
                shift,
                ..Modifiers::NONE
            };
            let press = |pressed| {
                Event::Key(KeyEvent {
                    key,
                    modifiers,
                    pressed,
                })
            };
            let mut this = self.send(press(true));
            if !c.is_control() {
                this = self.send(Event::Text(c)) && this;
            }
            self.send(press(false));
            used &= this;
        }
        used
    }

    /// Moves the keyboard focus forwards, as Tab does.
    pub fn tab(&mut self) -> bool {
        self.key(Key::Tab)
    }

    /// Moves the keyboard focus backwards, as Shift+Tab does.
    pub fn shift_tab(&mut self) -> bool {
        self.key_with(
            Key::Tab,
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
        )
    }

    /// Where the pointer was last sent.
    pub fn pointer(&self) -> Point {
        self.pointer
    }

    /// Changes the interface's size and scale.
    pub fn resize(&mut self, size: Size, scale: f32) {
        self.root.resize(size, scale);
    }

    // Output.

    /// The back buffer, as the last frame left it.
    pub fn pixels(&self) -> &Pixmap {
        self.root.pixels()
    }

    /// The scene of the last frame drawn whole into a new pixmap, with a
    /// new renderer: what the back buffer must equal, however many partial
    /// frames led to it.
    pub fn render_fresh(&self) -> Pixmap {
        let mut pixels = Pixmap::new(self.pixels().width(), self.pixels().height())
            .unwrap_or_else(|| self.pixels().clone());
        pixels.fill(manguebyte_gfx::Color::TRANSPARENT);
        Renderer::new().render(self.root.scene(), &mut pixels, self.root.scale());
        pixels
    }

    /// The exact text of the scene of the last frame.
    pub fn dump(&self) -> String {
        self.root.scene().dump()
    }

    /// The exact text of the scene `found` last recorded.
    pub fn dump_of(&self, found: &Found) -> String {
        self.root
            .widget_scene(found.id)
            .map(|scene| scene.dump())
            .unwrap_or_default()
    }
}

impl fmt::Debug for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Harness")
            .field("root", &self.root)
            .field("clock", &self.clock)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}
