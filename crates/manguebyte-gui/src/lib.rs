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
//! # The keyboard in hosts
//!
//! Hosts keep keys for their own shortcuts, and many give a plugin's
//! window the keyboard only when it is clicked, or never: the space bar
//! starts the transport, letters arm tracks. What reaches an editor depends
//! on the format, and the glue takes what each format gives:
//!
//! - **CLAP** has no keyboard in its interface: the editor's own window
//!   receives keys from the system. A press in the editor takes the
//!   keyboard for its window (`SetFocus` on Win32). A key, a typed
//!   character or a wheel turn the interface does not use goes on to the
//!   host's window with the same message, so the host's shortcuts keep
//!   working while the editor has the keyboard; a key whose press the
//!   interface used has its release kept too. A host that takes keys
//!   before its message loop hands them to the window (accelerator
//!   tables, hooks) still has them: CLAP has no call that lets a plugin
//!   ask for them.
//! - **VST 3** hosts may keep the keyboard and deliver keys through
//!   `IPlugView::onKeyDown` and `onKeyUp`. The wrapper hands them to the
//!   editor ([`OpenEditor::key`](manguebyte_plugin::editor::OpenEditor::key)),
//!   which gives each to the interface and answers `kResultTrue` for one it
//!   used, so the host does not also take it as a shortcut. While the
//!   interface takes typed text the character such a key carries is typed
//!   too, since no character message follows it, unless Control or
//!   Command make it a shortcut. Some hosts deliver a key both ways; a
//!   keystroke that arrives through the window and through the host before
//!   the next frame is taken once.
//! - **Typing**, on either: while the widget with the focus takes text (a
//!   text input, or a knob or slider whose value is being typed), the
//!   window asks for every key, so that a host that runs the editor inside
//!   a dialog keeps none of Tab, Enter and Escape (`WM_GETDLGCODE`), and
//!   the interface uses every key that types, moves or deletes, so the
//!   host sees none of them. When typing starts while the host has the
//!   keyboard (a value opened with Enter delivered through `onKeyDown`),
//!   the editor takes the keyboard for its window, so that the keys that
//!   follow come to it directly. An open menu is treated the same way: it
//!   takes every key until it closes. On Win32 the window keeps a hidden system
//!   caret where the interface's caret is, which screen magnifiers follow,
//!   and asks the input method to compose there.
//!
//! Which hosts take which keys can only be found by trying them; ADR 0007
//! keeps that as an open question for a manual matrix.
//!
//! # Windows
//!
//! Only Win32 parents have a backend so far. An editor asked to open in
//! any other kind of window says it does not support it
//! ([`EditorSpec::supports`](manguebyte_plugin::editor::EditorSpec::supports))
//! and refuses with
//! [`EditorError::Unsupported`](manguebyte_plugin::editor::EditorError::Unsupported).
//!
//! # Application windows
//!
//! A standalone program, or a plugin run as one, shows its interface in a
//! window of its own: [`Editor::open_in_app`] opens an editor in a window
//! of a `manguebyte_window::App`, and [`open_app_window`] opens any view.
//! Both return an [`AppEditor`], which runs the same glue with no host:
//! gestures set the parameters given, the window follows its monitor's
//! scale, a close request closes it unless the application says otherwise
//! ([`AppEditor::on_close_request`]), files dropped on it reach the widget
//! under the pointer as drags carrying a `DropData`, and the system's file
//! dialogs are a call away ([`AppEditor::dialogs`]). `App::run` runs the
//! loop. An editor inside a host takes drops too when built
//! [`with_file_drops`](Editor::with_file_drops). See ADR 0010.
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

mod app;
mod editor;
#[cfg(feature = "headless")]
mod headless;
mod open;

#[cfg(all(test, windows))]
mod win32_tests;

pub use app::{AppEditor, open_app_window};
pub use editor::Editor;
#[cfg(feature = "headless")]
pub use headless::HeadlessEditor;
pub use open::FRAME_PERIOD_MS;
