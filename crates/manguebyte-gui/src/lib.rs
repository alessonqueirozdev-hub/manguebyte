//! The glue between Manguebyte's plugin wrappers and its user interface.
//!
//! This crate is the last layer of ADR 0007. A plugin author describes an
//! editor with an [`Editor`]: its size in logical pixels, how it may be
//! resized, and a function that builds its view from `manguebyte-ui`
//! widgets for the plugin's parameters. `Editor` implements the editor
//! contract's [`EditorSpec`](manguebyte_plugin::editor::EditorSpec), so a
//! plugin returns it from
//! [`Plugin::editor`](manguebyte_plugin::Plugin::editor) and the CLAP and
//! VST 3 wrappers open it whenever the host asks.
//!
//! When the host opens it, the editor makes a child window inside the
//! host's window with `manguebyte-window`, builds a
//! [`Root`](manguebyte_ui::Root) on the
//! [`EditorContext`](manguebyte_plugin::editor::EditorContext) the wrapper
//! gave, sends the window's input to the root, runs a frame every
//! [`FRAME_PERIOD_MS`] milliseconds from a timer the window owns, and
//! presents only what each frame drew again. Edits reach the host only as
//! the root's gestures, through the context; parameters and meter levels
//! are read from their atomics once per frame. The audio thread never sees
//! any of it.
//!
//! # Windows
//!
//! Only Win32 parents have a backend so far. An editor asked to open in
//! any other kind of window says it does not support it
//! ([`EditorSpec::supports`](manguebyte_plugin::editor::EditorSpec::supports))
//! and refuses with
//! [`EditorError::Unsupported`](manguebyte_plugin::editor::EditorError::Unsupported).
//!
//! # Headless mode, for tests
//!
//! The `headless` cargo feature, which test code enables, adds
//! `Editor::on_headless_open` and `HeadlessEditor`. An editor given a hook
//! with `on_headless_open` opens in a headless window whatever its parent,
//! on every system, and hands the test a `HeadlessEditor` to drive it
//! with: input in logical pixels, the frame timer, the clock, and what the
//! editor shows. A test can then drive a plugin end to end through a real
//! wrapper's C or COM interface on a machine with no screen. The feature
//! only adds API: an editor not given a hook opens in the system's window
//! whether the feature is on or not, so a build that turns it on through
//! another crate changes no editor.
//!
//! # Hygiene
//!
//! The crate has no `unsafe` code, keeps no global state and starts no
//! thread; what it asks of the system goes through `manguebyte-window`. It
//! is never a dependency of a crate that processes audio.

#![forbid(unsafe_code)]

mod editor;
#[cfg(feature = "headless")]
mod headless;
mod open;

#[cfg(all(test, windows))]
mod win32_tests;

pub use editor::Editor;
#[cfg(feature = "headless")]
pub use headless::HeadlessEditor;
pub use open::FRAME_PERIOD_MS;
