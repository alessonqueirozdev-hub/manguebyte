//! A controller for an editor opened in headless mode, for tests.

use std::cell::Cell;
use std::fmt;
use std::rc::{Rc, Weak};
use std::time::Duration;

use manguebyte_gfx::{Pixmap, Point};
use manguebyte_plugin::editor::{Key, KeyEvent, Modifiers};
use manguebyte_ui::{AccessEntry, Role};
use manguebyte_window::{
    Button, DragData, Handled as WindowHandled, Headless, PhysicalPoint, PhysicalSize, RawInput,
};

use crate::open::{Clock, FRAME_PERIOD_MS, Shared};

/// An editor that opened in headless mode, as a test drives it.
///
/// An [`Editor`](crate::Editor) given a hook with
/// [`Editor::on_headless_open`](crate::Editor::on_headless_open) (with the
/// `headless` feature) opens in a window with no system window behind it,
/// whatever parent the host gives, and hands one of these to the hook.
/// Through
/// it a test plays the person and the system: it sends pointer and key
/// input to the window in logical pixels, runs the window's frame timer,
/// and reads what the editor shows, its scene and its accessibility tree.
/// Everything it does reaches the editor through the same window code as
/// a system window's input does.
///
/// The editor's clock is the test's: it starts at zero and moves only
/// with [`tick`](Self::tick) and [`advance`](Self::advance).
///
/// It holds the editor weakly. Once the wrapper drops the editor, input
/// does nothing and the readers return nothing.
///
/// ```
/// use std::cell::RefCell;
/// use std::rc::Rc;
/// use manguebyte_gui::{Editor, HeadlessEditor};
/// use manguebyte_plugin::editor::testing::RecordingEditorHost;
/// use manguebyte_plugin::editor::{EditorContext, EditorSpec, HostSize, ParentWindow};
/// use manguebyte_ui::{Role, label};
///
/// thread_local! {
///     static OPENED: RefCell<Option<HeadlessEditor>> = const { RefCell::new(None) };
/// }
///
/// let spec = Editor::new(HostSize::new(120, 40), || label("Hello"))
///     .on_headless_open(|editor| OPENED.with(|slot| *slot.borrow_mut() = Some(editor)));
/// let (context, link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
/// let parent = ParentWindow::win32(0x10).expect("not null");
/// let mut editor = spec.open(parent, Some(2.0), context).expect("headless windows always open");
///
/// let ui = OPENED.with(|slot| slot.borrow_mut().take()).expect("the hook ran");
/// // Hidden until the host shows it.
/// assert_eq!(ui.tick(), 0, "frames ran before the editor was shown");
/// editor.set_visible(true);
/// assert_eq!(ui.tick(), 1, "the frame timer fired");
/// let hello = ui.find(Role::Label, "Hello").expect("drawn");
/// assert!(hello.device.width > hello.bounds.width as u32, "at 2x");
/// let shown = ui.shown().expect("open");
/// assert_eq!((shown.width(), shown.height()), (240, 80));
///
/// drop(editor);
/// drop(link);
/// assert!(!ui.is_open());
/// ```
pub struct HeadlessEditor {
    window: Headless,
    shared: Weak<Shared>,
    /// The time of the next press, for click counting: each press is a
    /// second after the last, so no two count as a double click.
    press_time: Cell<u32>,
}

impl HeadlessEditor {
    pub(crate) fn new(window: Headless, shared: Weak<Shared>) -> HeadlessEditor {
        HeadlessEditor {
            window,
            shared,
            press_time: Cell::new(0),
        }
    }

    fn shared(&self) -> Option<Rc<Shared>> {
        self.shared.upgrade()
    }

    /// The headless window itself, for what the methods here do not cover:
    /// raw input, exposes, resizes as a host does them, the system's
    /// scale, destroying the parent, and what was copied to the "screen".
    pub fn window(&self) -> &Headless {
        &self.window
    }

    /// Whether the editor is still open and has its interface: false once
    /// the wrapper has dropped it, or after its window was destroyed under
    /// it.
    pub fn is_open(&self) -> bool {
        self.shared()
            .is_some_and(|shared| shared.root.try_borrow().is_ok_and(|root| root.is_some()))
    }

    /// The time on the editor's clock.
    pub fn now(&self) -> Duration {
        self.shared()
            .map_or(Duration::ZERO, |shared| shared.clock.now())
    }

    /// Moves the editor's clock on by `by`, without running a frame.
    pub fn advance(&self, by: Duration) {
        if let Some(shared) = self.shared() {
            if let Clock::Manual(now) = &shared.clock {
                now.set(now.get().saturating_add(by));
            }
        }
    }

    /// Moves the clock on by one frame period and fires the window's
    /// timers, as the system would. Returns how many fired: 1 while the
    /// editor is visible, 0 while it is hidden or gone.
    pub fn tick(&self) -> usize {
        self.advance(Duration::from_millis(u64::from(FRAME_PERIOD_MS)));
        self.window.fire_timers()
    }

    /// How many frames have run, and how many of them presented something.
    pub fn frames(&self) -> (u64, u64) {
        self.shared().map_or((0, 0), |shared| {
            (shared.frames.get(), shared.presented.get())
        })
    }

    /// Over all frames: how many widgets recorded a new scene, and how many
    /// frames laid the interface out. A frame where nothing changed adds to
    /// neither; it only polls the parameters and sources.
    pub fn work(&self) -> (u64, u64) {
        self.shared().map_or((0, 0), |shared| {
            (shared.painted.get(), shared.layouts.get())
        })
    }

    /// What the window shows, as premultiplied RGBA, or `None` once it is
    /// gone.
    pub fn shown(&self) -> Option<Pixmap> {
        self.window.frame()
    }

    /// The interface's back buffer, as the last frame left it.
    pub fn pixels(&self) -> Option<Pixmap> {
        self.with_root(|root| root.pixels().clone())
    }

    /// The exact text of the scene the back buffer was last drawn from.
    pub fn dump(&self) -> Option<String> {
        self.with_root(|root| root.scene().dump())
    }

    /// Every widget's description, in tree order.
    pub fn access_tree(&self) -> Vec<AccessEntry> {
        self.with_root(|root| root.access_tree())
            .unwrap_or_default()
    }

    /// The widget with `role` and `name`, if there is exactly one.
    pub fn find(&self, role: Role, name: &str) -> Option<AccessEntry> {
        let mut found: Vec<AccessEntry> = self
            .access_tree()
            .into_iter()
            .filter(|entry| entry.node.role == role && entry.node.name == name)
            .collect();
        if found.len() == 1 { found.pop() } else { None }
    }

    /// The interface's size in logical pixels and its scale.
    pub fn layout(&self) -> Option<(manguebyte_gfx::Size, f32)> {
        self.with_root(|root| (root.size(), root.scale()))
    }

    /// The window's size in device pixels, or `None` once it is gone.
    pub fn size(&self) -> Option<PhysicalSize> {
        self.window
            .frame()
            .map(|frame| PhysicalSize::new(frame.width(), frame.height()))
    }

    fn with_root<T>(&self, read: impl FnOnce(&manguebyte_ui::Root) -> T) -> Option<T> {
        let shared = self.shared()?;
        let slot = shared.root.try_borrow().ok()?;
        slot.as_ref().map(read)
    }

    /// `at` in device pixels, at the interface's scale.
    fn device(&self, at: Point) -> PhysicalPoint {
        let scale = self.layout().map_or(1.0, |(_, scale)| f64::from(scale));
        let device = |v: f32| (f64::from(v) * scale).round() as i32;
        PhysicalPoint::new(device(at.x), device(at.y))
    }

    /// Drags `data` from another program onto the editor at `at`, in
    /// logical pixels, and returns whether a widget takes it there: never
    /// when the editor's window takes no drops.
    pub fn drag_enter(&self, at: Point, data: DragData) -> bool {
        self.window.drag_enter(self.device(at), data) == WindowHandled::Yes
    }

    /// Moves the drag to `at`; returns whether a widget takes it there.
    pub fn drag_over(&self, at: Point) -> bool {
        self.window.drag_over(self.device(at)) == WindowHandled::Yes
    }

    /// Drops the drag at `at`; returns whether a widget took it.
    pub fn drop_at(&self, at: Point) -> bool {
        self.window.drop_at(self.device(at)) == WindowHandled::Yes
    }

    /// The drag leaves the editor, or is cancelled.
    pub fn drag_leave(&self) {
        self.window.drag_leave();
    }

    /// Presses the main button at `at`, in logical pixels.
    pub fn press(&self, at: Point) {
        let time_ms = self.press_time.get();
        self.press_time.set(time_ms.wrapping_add(1000));
        self.window.input(RawInput::PointerDown {
            at: self.device(at),
            button: Button::Primary,
            modifiers: Modifiers::NONE,
            time_ms,
        });
    }

    /// Moves the pointer to `at`, in logical pixels.
    pub fn move_to(&self, at: Point) {
        self.window.input(RawInput::PointerMove {
            at: self.device(at),
            modifiers: Modifiers::NONE,
        });
    }

    /// Releases the main button at `at`, in logical pixels.
    pub fn release(&self, at: Point) {
        self.window.input(RawInput::PointerUp {
            at: self.device(at),
            button: Button::Primary,
            modifiers: Modifiers::NONE,
        });
    }

    /// Presses and releases `key`. Returns whether the editor used the
    /// press.
    pub fn key(&self, key: Key) -> bool {
        self.key_with(key, Modifiers::NONE)
    }

    /// Presses and releases `key` with `modifiers` held. Returns whether
    /// the editor used the press.
    pub fn key_with(&self, key: Key, modifiers: Modifiers) -> bool {
        let send = |pressed| {
            self.window.input(RawInput::Key {
                key: KeyEvent {
                    key,
                    modifiers,
                    pressed,
                },
                repeat: false,
            })
        };
        let used = send(true) == Some(manguebyte_window::Handled::Yes);
        send(false);
        used
    }

    /// Types `text` as the system delivers a keyboard's typing to the
    /// window: for each character, its key goes down, the character comes
    /// as UTF-16 units (two for a character outside the Basic Multilingual
    /// Plane), and the key comes up. Returns whether every character was
    /// used.
    pub fn type_text(&self, text: &str) -> bool {
        let mut used = true;
        for c in text.chars() {
            let modifiers = Modifiers {
                shift: c.is_uppercase(),
                ..Modifiers::NONE
            };
            let key = Key::Character(c.to_lowercase().next().unwrap_or(c));
            let send = |pressed| {
                self.window.input(RawInput::Key {
                    key: KeyEvent {
                        key,
                        modifiers,
                        pressed,
                    },
                    repeat: false,
                })
            };
            send(true);
            let mut units = [0u16; 2];
            let mut typed = None;
            for unit in c.encode_utf16(&mut units) {
                typed = self.window.input(RawInput::Utf16(*unit));
            }
            used &= typed == Some(manguebyte_window::Handled::Yes);
            send(false);
        }
        used
    }

    /// Two presses and releases of the main button at `at`, close enough
    /// in time to count as a double click.
    pub fn double_click(&self, at: Point) {
        let first = self.press_time.get();
        self.press(at);
        self.release(at);
        self.press_time.set(first.wrapping_add(1));
        self.press(at);
        self.release(at);
        self.press_time.set(first.wrapping_add(2000));
    }
}

impl fmt::Debug for HeadlessEditor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeadlessEditor")
            .field("open", &self.is_open())
            .finish_non_exhaustive()
    }
}
