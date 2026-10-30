//! Sources: where the values that widgets show come from.
//!
//! The interface has no listeners and no callbacks into widgets (ADR 0003,
//! extended to the interface by ADR 0007). A widget that shows a value
//! reads it from a [`Source`] once per frame and compares it with the last
//! value it saw; only a value that changed makes it draw again. Parameters,
//! meter levels the audio thread publishes, and the interface's own state
//! all work this way, so there is no order of notification to get wrong
//! and nothing can call back into a widget while it is busy.
//!
//! - Any closure `Fn() -> T` is a source.
//! - [`AtomicSource`] reads an `f32` that another thread, usually the audio
//!   thread, stores into a [`manguebyte_core::AtomicF32`]. The other
//!   thread needs nothing from this crate.
//! - [`State`] is a value owned by the interface itself, such as a selected
//!   tab, with a version number so that a widget can tell it changed
//!   without copying it. A plugin's editor spec can keep it, so that it
//!   survives closing and reopening the editor.
//! - [`Watch`] keeps a source and the last value read from it.
//! - [`TextSource`] is a text a widget shows or is named by: fixed, or
//!   from a source or a state, so that names can change while the
//!   interface is open.
//!
//! ```
//! use std::sync::Arc;
//! use std::sync::atomic::Ordering;
//! use manguebyte_core::AtomicF32;
//! use manguebyte_ui::source::{AtomicSource, Source, State, Watch};
//!
//! // What the audio thread writes, once per block.
//! let level = Arc::new(AtomicF32::new(-60.0));
//! let mut watch = Watch::new(AtomicSource::new(Arc::clone(&level)));
//! assert!(watch.poll(), "the first read counts as a change");
//! assert!(!watch.poll(), "nothing changed");
//! level.store(-12.0, Ordering::Relaxed);
//! assert!(watch.poll());
//! assert_eq!(watch.get(), Some(&-12.0));
//!
//! // The interface's own state, shared by two widgets.
//! let tab = State::new(0_u32);
//! let other = tab.clone();
//! let seen = tab.version();
//! other.set(2);
//! assert_ne!(tab.version(), seen);
//! assert_eq!(tab.read(), 2);
//! ```

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use manguebyte_core::AtomicF32;

/// A value that can be read at any time, on the interface's thread.
///
/// Reading should be cheap, because it happens on every frame: an atomic
/// load or a few arithmetic operations. It must not have side effects that
/// matter, because how often it happens depends on the frame rate.
pub trait Source<T>: 'static {
    /// The value now.
    fn read(&self) -> T;
}

impl<T, F: Fn() -> T + 'static> Source<T> for F {
    fn read(&self) -> T {
        self()
    }
}

/// An `f32` that another thread publishes through an atomic.
///
/// The audio thread holds the other `Arc` and calls
/// [`AtomicF32::store`] once per block with `Ordering::Relaxed`, which is
/// real-time safe; it depends on `manguebyte-core` only, never on the
/// interface. The interface reads the latest value once per frame. Values
/// stored between two frames are not seen; a meter that needs peaks keeps
/// the peak on the audio side.
#[derive(Clone)]
pub struct AtomicSource {
    value: Arc<AtomicF32>,
}

impl AtomicSource {
    /// A source that reads `value`.
    pub fn new(value: Arc<AtomicF32>) -> Self {
        AtomicSource { value }
    }
}

impl Source<f32> for AtomicSource {
    fn read(&self) -> f32 {
        self.value.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for AtomicSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtomicSource")
            .field("value", &self.read())
            .finish()
    }
}

/// A source and the last value read from it.
pub struct Watch<T> {
    source: Box<dyn Source<T>>,
    last: Option<T>,
}

impl<T: PartialEq + 'static> Watch<T> {
    /// Watches `source`. Nothing has been read yet.
    pub fn new(source: impl Source<T>) -> Self {
        Watch {
            source: Box::new(source),
            last: None,
        }
    }

    /// Reads the source and returns whether the value differs from the
    /// last one read. The first read counts as a change.
    pub fn poll(&mut self) -> bool {
        let value = self.source.read();
        if self.last.as_ref() == Some(&value) {
            return false;
        }
        self.last = Some(value);
        true
    }

    /// The last value read, if any has been.
    pub fn get(&self) -> Option<&T> {
        self.last.as_ref()
    }
}

impl<T: fmt::Debug> fmt::Debug for Watch<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Watch")
            .field("last", &self.last)
            .finish_non_exhaustive()
    }
}

/// A text that a widget shows or is named by: fixed, or read once per
/// frame from a [`Source`] or a [`State`], so that it can change while
/// the interface is open (the language, say).
///
/// A widget [`poll`](TextSource::poll)s it in its
/// [`update`](crate::Widget::update) and, when the text changed, lays out
/// again if its size depends on the text, draws again if it shows it, and
/// asks for its description to be sent to assistive technology again
/// ([`UpdateCx::request_access_update`](crate::UpdateCx::request_access_update)).
///
/// Polling does not touch the heap unless the source does:
///
/// - a fixed text is never read again;
/// - a [`State`] is read again only when its version changed, and its
///   text is kept shared, not copied;
/// - a source is read every frame and compared with the last text: one
///   that returns a `&'static str` from a table, or clones an `Arc<str>`,
///   makes no heap call, while a closure that builds a `String` builds one
///   every frame.
///
/// A text from a source or a state is read once when it is made, so that
/// it is there before the first frame, and the first poll reports no
/// change.
///
/// ```
/// use manguebyte_ui::{State, TextSource};
///
/// // A language the person picks, and a table of texts in each.
/// let language = State::new(0_usize);
/// let read = language.clone();
/// let mut name = TextSource::dynamic(move || ["Cutoff", "Corte"][read.get()]);
/// assert_eq!(name.as_str(), "Cutoff");
/// assert!(!name.poll(), "read when made");
/// language.set(1);
/// assert!(name.poll());
/// assert_eq!(name.as_str(), "Corte");
///
/// let shared = State::new(String::from("Mix"));
/// let mut name = TextSource::from(shared.clone());
/// shared.set(String::from("Mix"));
/// assert!(!name.poll(), "the same text is no change");
/// shared.set(String::from("Mistura"));
/// assert!(name.poll());
/// assert_eq!(name.as_str(), "Mistura");
/// ```
pub struct TextSource {
    kind: TextKind,
}

enum TextKind {
    Fixed(String),
    Polled(Box<dyn PolledText>),
    Shared {
        state: State<String>,
        seen: u64,
        text: Arc<String>,
    },
}

/// A watched source of something that reads as text.
trait PolledText {
    fn poll(&mut self) -> bool;
    fn text(&self) -> &str;
}

impl<T: AsRef<str> + PartialEq + 'static> PolledText for Watch<T> {
    fn poll(&mut self) -> bool {
        Watch::poll(self)
    }

    fn text(&self) -> &str {
        self.get().map_or("", AsRef::as_ref)
    }
}

impl TextSource {
    /// `text`, which never changes.
    pub fn fixed(text: impl Into<String>) -> TextSource {
        TextSource {
            kind: TextKind::Fixed(text.into()),
        }
    }

    /// What `source` gives, read now and then once per frame. Anything
    /// that reads as text will do: `String`, `&'static str`, `Arc<str>`.
    pub fn dynamic<T: AsRef<str> + PartialEq + 'static>(source: impl Source<T>) -> TextSource {
        let mut watch = Watch::new(source);
        watch.poll();
        TextSource {
            kind: TextKind::Polled(Box::new(watch)),
        }
    }

    /// The text in `state`, read again only when its version changes.
    pub fn from_state(state: State<String>) -> TextSource {
        TextSource {
            kind: TextKind::Shared {
                seen: state.version(),
                text: state.load(),
                state,
            },
        }
    }

    /// Reads the text again and returns whether it differs from the one
    /// read before. A fixed text never changes.
    pub fn poll(&mut self) -> bool {
        match &mut self.kind {
            TextKind::Fixed(_) => false,
            TextKind::Polled(watch) => watch.poll(),
            TextKind::Shared { state, seen, text } => {
                let version = state.version();
                if version == *seen {
                    return false;
                }
                *seen = version;
                let new = state.load();
                let changed = **text != *new;
                *text = new;
                changed
            }
        }
    }

    /// The text, as last read.
    pub fn as_str(&self) -> &str {
        match &self.kind {
            TextKind::Fixed(text) => text,
            TextKind::Polled(watch) => watch.text(),
            TextKind::Shared { text, .. } => text,
        }
    }

    /// Whether the text is fixed, and so never changes.
    pub fn is_fixed(&self) -> bool {
        matches!(self.kind, TextKind::Fixed(_))
    }
}

impl From<String> for TextSource {
    fn from(text: String) -> TextSource {
        TextSource::fixed(text)
    }
}

impl From<&str> for TextSource {
    fn from(text: &str) -> TextSource {
        TextSource::fixed(text)
    }
}

impl From<&String> for TextSource {
    fn from(text: &String) -> TextSource {
        TextSource::fixed(text.as_str())
    }
}

impl From<State<String>> for TextSource {
    fn from(state: State<String>) -> TextSource {
        TextSource::from_state(state)
    }
}

impl fmt::Debug for TextSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            TextKind::Fixed(_) => "fixed",
            TextKind::Polled(_) => "source",
            TextKind::Shared { .. } => "state",
        };
        f.debug_struct("TextSource")
            .field("text", &self.as_str())
            .field("from", &kind)
            .finish()
    }
}

/// A value owned by the interface, shared by the widgets that show or
/// change it, with a version that grows by one on every change.
///
/// Cloning a `State` shares the value. A widget remembers the
/// [`version`](State::version) it last showed and draws again when it
/// differs, so it never copies the value to find out. Nothing holds the
/// value locked while user code runs, so reading or setting a state from
/// inside [`with`](State::with) or [`update`](State::update) works and
/// cannot deadlock or panic.
///
/// A state of a value that is `Send` and `Sync` is `Send` and `Sync`
/// itself, so a plugin's
/// [`EditorSpec`](manguebyte_plugin::editor::EditorSpec), which must be,
/// can keep one and give a clone to each interface it opens: what the
/// person chose (a tab, a zoom) survives closing and reopening the editor.
/// Reading and setting take a lock for as long as it takes to copy a
/// pointer; it is meant for the interface's thread and the spec, never for
/// the audio thread, which shares values through atomics.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Label, Role, State};
///
/// // Kept by the editor spec, which outlives every open editor.
/// let page = State::new(String::from("Page 1"));
/// let open = |page: &State<String>| {
///     let mut ui = Harness::new(Label::from_state(page.clone()), Size::new(100.0, 20.0), 1.0);
///     ui.frame();
///     ui
/// };
/// let ui = open(&page);
/// page.set(String::from("Page 2"));
/// drop(ui);
/// // Reopened, the interface shows what was chosen before.
/// assert!(open(&page).find(Role::Label, "Page 2").is_some());
///
/// fn kept_by_a_spec<T: Send + Sync + 'static>() {}
/// kept_by_a_spec::<State<String>>();
/// ```
pub struct State<T> {
    cell: Arc<StateCell<T>>,
}

struct StateCell<T> {
    value: Mutex<Arc<T>>,
    version: AtomicU64,
}

impl<T: 'static> State<T> {
    /// A state holding `value`, at version 0.
    pub fn new(value: T) -> Self {
        State {
            cell: Arc::new(StateCell {
                value: Mutex::new(Arc::new(value)),
                version: AtomicU64::new(0),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Arc<T>> {
        // Nothing that can panic runs while the lock is held, so it is
        // never poisoned; if it were, the pointer in it is still whole.
        self.cell
            .value
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The value, shared.
    fn load(&self) -> Arc<T> {
        Arc::clone(&self.lock())
    }

    fn store(&self, value: Arc<T>) {
        let old = std::mem::replace(&mut *self.lock(), value);
        // Counted once the new value is in place, so a reader that sees
        // the new version reads the new value.
        self.cell.version.fetch_add(1, Ordering::AcqRel);
        // Dropped with the lock released: the value's `Drop` may read
        // this state.
        drop(old);
    }

    /// Calls `look` with the value and returns what it returns.
    pub fn with<R>(&self, look: impl FnOnce(&T) -> R) -> R {
        look(&self.load())
    }

    /// Replaces the value.
    pub fn set(&self, value: T) {
        self.store(Arc::new(value));
    }

    /// How many times the value has been changed.
    pub fn version(&self) -> u64 {
        self.cell.version.load(Ordering::Acquire)
    }
}

impl<T: Clone + 'static> State<T> {
    /// A copy of the value.
    pub fn get(&self) -> T {
        T::clone(&self.load())
    }

    /// Changes the value in place with `change`, which works on a copy:
    /// what it reads of this state meanwhile is the value before.
    pub fn update(&self, change: impl FnOnce(&mut T)) {
        let mut value = self.get();
        change(&mut value);
        self.set(value);
    }
}

impl<T> Clone for State<T> {
    fn clone(&self) -> Self {
        State {
            cell: Arc::clone(&self.cell),
        }
    }
}

impl<T: Clone + 'static> Source<T> for State<T> {
    fn read(&self) -> T {
        self.get()
    }
}

impl<T: fmt::Debug + 'static> fmt::Debug for State<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.load();
        f.debug_struct("State")
            .field("value", &value)
            .field("version", &self.version())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_can_be_read_and_set_from_inside_its_own_closures() {
        let state = State::new(String::from("a"));
        let inner = state.clone();
        state.with(|value| {
            assert_eq!(value, "a");
            inner.set(format!("{value}b"));
            assert_eq!(inner.get(), "ab");
        });
        state.update(|value| {
            // The copy being changed is not visible yet.
            assert_eq!(inner.get(), "ab");
            value.push('c');
        });
        assert_eq!(state.get(), "abc");
        assert_eq!(state.version(), 2);
    }

    #[test]
    fn a_state_can_be_kept_and_set_on_another_thread() {
        let state = State::new(0_u32);
        let kept = state.clone();
        let seen = state.version();
        std::thread::spawn(move || {
            for value in 1..=100 {
                kept.set(value);
            }
        })
        .join()
        .expect("the other thread ran");
        assert_eq!(state.get(), 100);
        assert_eq!(state.version(), seen + 100);
    }

    #[test]
    fn a_watch_reports_each_change_once() {
        let state = State::new(1);
        let source = state.clone();
        let mut watch = Watch::new(move || source.get() * 10);
        assert!(watch.poll());
        assert!(!watch.poll());
        state.set(1);
        assert!(!watch.poll(), "the same value is no change");
        state.set(2);
        assert!(watch.poll());
        assert_eq!(watch.get(), Some(&20));
    }
}
