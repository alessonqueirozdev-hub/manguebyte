//! A hosted plugin's editor: opened inside a window the application owns,
//! sized and scaled at the application's word, and kept running by the
//! timers and descriptors it registered.
//!
//! Every call into the plugin's `gui`, `timer-support` and
//! `posix-fd-support` is made here, on the main thread, from a method of
//! [`ClapPlugin`], with nothing locked that a callback needs. The callbacks
//! touch only [`Shared`](super::super::host::Shared), and only briefly, so
//! the plugin may call the host back from inside any of these calls: ask
//! for a new size from inside `set_scale` or `set_size`, register a timer
//! from inside `set_parent`, unregister one from inside `on_timer`. What it
//! asks the application for is answered later, by
//! [`pump`](ClapPlugin::pump), never from inside the request.

use std::ffi::CStr;
use std::ptr;
use std::time::Instant;

use clap_sys::ext::gui::{
    CLAP_WINDOW_API_COCOA, CLAP_WINDOW_API_WIN32, CLAP_WINDOW_API_X11, clap_plugin_gui,
    clap_window, clap_window_handle,
};

use super::{ClapPlugin, HostEvent};
use crate::clap::ClapError;
use crate::clap::host::{editor_request, fd_flags};
use crate::editor::{EditorRequest, EditorSize, FdInterest, FdWatch, ParentWindow, WindowApi};

/// The editor while it is open, as the host keeps it.
pub(super) struct OpenEditor {
    /// The kind of window it is in.
    api: WindowApi,
    /// Its size, as last learned from the plugin or given to it.
    size: EditorSize,
}

/// Whether a plugin's `gui` table has every function this host needs to
/// show an editor inside a window: the rest are optional, and a call to
/// one the plugin left out is answered as a refusal.
pub(super) fn is_complete(gui: &clap_plugin_gui) -> bool {
    gui.is_api_supported.is_some()
        && gui.create.is_some()
        && gui.destroy.is_some()
        && gui.get_size.is_some()
        && gui.set_parent.is_some()
}

/// The name CLAP gives a kind of window.
fn api_name(api: WindowApi) -> Option<&'static CStr> {
    match api {
        WindowApi::Win32 => Some(CLAP_WINDOW_API_WIN32),
        WindowApi::Cocoa => Some(CLAP_WINDOW_API_COCOA),
        WindowApi::X11 => Some(CLAP_WINDOW_API_X11),
        _ => None,
    }
}

/// The kind of window an application on this system puts an editor in.
fn platform_api() -> WindowApi {
    if cfg!(target_os = "windows") {
        WindowApi::Win32
    } else if cfg!(target_os = "macos") {
        WindowApi::Cocoa
    } else {
        WindowApi::X11
    }
}

/// An X11 window id as CLAP passes it: an `unsigned long`, which is 32 bits
/// wide on some systems and 64 on others.
fn x11_window<T: From<u32>>(window: u32) -> T {
    T::from(window)
}

/// `parent` as CLAP describes a window to a plugin.
fn clap_window_of(parent: ParentWindow, api: &'static CStr) -> Option<clap_window> {
    let specific = match parent {
        ParentWindow::Win32 { hwnd } => clap_window_handle {
            win32: ptr::with_exposed_provenance_mut(hwnd.get()),
        },
        ParentWindow::Cocoa { ns_view } => clap_window_handle {
            cocoa: ptr::with_exposed_provenance_mut(ns_view.get()),
        },
        ParentWindow::X11 { window } => clap_window_handle {
            x11: x11_window(window.get()),
        },
        _ => return None,
    };
    Some(clap_window {
        api: api.as_ptr(),
        specific,
    })
}

/// Whether a scale is one a plugin can be given.
fn is_scale(scale: f64) -> bool {
    scale.is_finite() && scale > 0.0
}

const NOT_A_SCALE: ClapError = ClapError::InvalidArgument("a scale has to be a positive number");

impl ClapPlugin {
    /// Whether the plugin has an editor that can go inside a window of the
    /// kind applications on this system use: Win32 on Windows, Cocoa on
    /// macOS, X11 elsewhere. Embedded only; this host does not ask for the
    /// floating windows CLAP also knows.
    pub fn has_editor(&self) -> bool {
        let (Some(gui), Some(name)) = (self.extensions.gui, api_name(platform_api())) else {
            return false;
        };
        self.takes_api(&gui, name)
    }

    /// Whether the plugin's editor can go inside a window of the API
    /// `name`, embedded.
    fn takes_api(&self, gui: &clap_plugin_gui, name: &CStr) -> bool {
        let Some(supported) = gui.is_api_supported else {
            return false;
        };
        // SAFETY: the plugin is initialised, this is its main thread, and
        // `name` is a C string that outlives the call.
        unsafe { supported(self.plugin, name.as_ptr(), false) }
    }

    /// Opens the plugin's editor inside `parent`, shows it, and returns its
    /// size. Make `parent` that size: the editor is drawn at the top left
    /// of it and does not grow or shrink with it by itself.
    ///
    /// `scale` is the factor the editor is to draw at, 2 for twice the
    /// size, for a Win32 or X11 window; the plugin is free to work it out
    /// from the system instead, and to refuse it. A Cocoa view counts in
    /// points, which the system scales, so there `scale` is not passed on,
    /// as CLAP asks.
    ///
    /// This is CLAP's order of calls: `create`, `set_scale`, `get_size`,
    /// `set_parent`, `show`. If any of them fails, whatever was created is
    /// destroyed again, so nothing is left half open.
    ///
    /// While the editor is open, call [`pump`](Self::pump) as for every
    /// plugin, at the latest by [`next_editor_timer`](Self::next_editor_timer),
    /// and act on the [`HostEvent::Editor`] requests it returns.
    ///
    /// # Errors
    ///
    /// - [`ClapError::EditorAlreadyOpen`] if it is open.
    /// - [`ClapError::NoEditor`] if the plugin has none.
    /// - [`ClapError::EditorApiUnsupported`] if it cannot go inside a
    ///   window of `parent`'s kind.
    /// - [`ClapError::InvalidArgument`] for a scale that is not a positive
    ///   number.
    /// - [`ClapError::EditorRefused`] if the plugin refused one of the
    ///   calls; the text names it.
    pub fn open_editor(
        &mut self,
        parent: ParentWindow,
        scale: Option<f64>,
    ) -> Result<EditorSize, ClapError> {
        if self.editor.is_some() {
            return Err(ClapError::EditorAlreadyOpen);
        }
        let gui = self.extensions.gui.ok_or(ClapError::NoEditor)?;
        let api = parent.api();
        let unsupported = ClapError::EditorApiUnsupported(api);
        let name = api_name(api).ok_or(unsupported.clone())?;
        if scale.is_some_and(|scale| !is_scale(scale)) {
            return Err(NOT_A_SCALE);
        }
        let window = clap_window_of(parent, name).ok_or(unsupported.clone())?;
        if !self.takes_api(&gui, name) {
            return Err(unsupported);
        }
        let (Some(create), Some(set_parent)) = (gui.create, gui.set_parent) else {
            return Err(ClapError::NoEditor);
        };

        // From here on the plugin's requests about its editor are taken.
        self.shared.set_editor_created(true);
        // SAFETY: the plugin is initialised, this is its main thread, it
        // has no editor, and `name` is a C string that outlives the call.
        if !unsafe { create(self.plugin, name.as_ptr(), false) } {
            self.shared.set_editor_created(false);
            self.shared.editor_loop().forget_editor();
            return Err(ClapError::EditorRefused("create"));
        }
        let opened = (|| {
            if let (Some(scale), Some(set_scale)) = (scale, gui.set_scale) {
                if api != WindowApi::Cocoa {
                    // A plugin that works its scale out by itself refuses,
                    // which CLAP allows.
                    // SAFETY: the plugin's editor is created, and this is
                    // its main thread.
                    unsafe { set_scale(self.plugin, scale) };
                }
            }
            let size = self
                .editor_size_now(&gui)
                .ok_or(ClapError::EditorRefused("get_size"))?;
            // SAFETY: the plugin's editor is created, this is its main
            // thread, and `window` describes the application's window
            // truthfully and outlives the call.
            if !unsafe { set_parent(self.plugin, &raw const window) } {
                return Err(ClapError::EditorRefused("set_parent"));
            }
            if let Some(show) = gui.show {
                // SAFETY: as above, with the editor now in its window.
                if !unsafe { show(self.plugin) } {
                    return Err(ClapError::EditorRefused("show"));
                }
            }
            Ok(size)
        })();
        match opened {
            Ok(size) => {
                self.editor = Some(OpenEditor { api, size });
                Ok(size)
            }
            Err(error) => {
                self.discard_editor(&gui, false);
                Err(error)
            }
        }
    }

    /// The editor's size as the plugin gives it now, or `None` if it will
    /// not say or says it has none.
    fn editor_size_now(&self, gui: &clap_plugin_gui) -> Option<EditorSize> {
        let get_size = gui.get_size?;
        let (mut width, mut height) = (0, 0);
        // SAFETY: the plugin's editor is created, this is its main thread,
        // and the two numbers belong to this function.
        if !unsafe { get_size(self.plugin, &raw mut width, &raw mut height) } {
            return None;
        }
        (width > 0 && height > 0).then_some(EditorSize::new(width, height))
    }

    /// Whether the editor is open.
    pub fn is_editor_open(&self) -> bool {
        self.editor.is_some()
    }

    /// The editor's size while it is open: what it opened at, or what it
    /// was last given or reported. A size the editor asks for with
    /// [`EditorRequest::Resize`] is its size only once the application
    /// has passed it to [`set_editor_size`](Self::set_editor_size).
    pub fn editor_size(&self) -> Option<EditorSize> {
        self.editor.as_ref().map(|editor| editor.size)
    }

    /// Whether the open editor can take sizes other than its own: false
    /// for an editor of fixed size, and when it is closed. The plugin is
    /// asked each time, as its answer may change with its scale.
    pub fn can_resize_editor(&self) -> bool {
        if self.editor.is_none() {
            return false;
        }
        let Some(can_resize) = self.extensions.gui.and_then(|gui| gui.can_resize) else {
            return false;
        };
        // SAFETY: the plugin's editor is created, and this is its main
        // thread.
        unsafe { can_resize(self.plugin) }
    }

    /// Tells the editor that its window is now `size`, and returns the
    /// size it takes, which the application then gives the window.
    ///
    /// This is how the application answers an [`EditorRequest::Resize`],
    /// and what it calls when the person resizes the window around the
    /// editor. An editor that can be resized first fits `size` to its
    /// rules (`adjust_size`: a step, an aspect ratio, a smallest size),
    /// then takes the fitted size. An editor of fixed size keeps the size
    /// it has, and that is what is returned, for the window to go back to.
    ///
    /// # Errors
    ///
    /// [`ClapError::EditorNotOpen`] if it is closed,
    /// [`ClapError::InvalidArgument`] for a size of zero either way, and
    /// [`ClapError::EditorRefused`] if a resizable editor refuses the
    /// size.
    pub fn set_editor_size(&mut self, size: EditorSize) -> Result<EditorSize, ClapError> {
        let (Some(editor), Some(gui)) = (&self.editor, self.extensions.gui) else {
            return Err(ClapError::EditorNotOpen);
        };
        let kept = editor.size;
        if size.width == 0 || size.height == 0 {
            return Err(ClapError::InvalidArgument(
                "an editor is at least one pixel wide and high",
            ));
        }
        let applied = if self.can_resize_editor() {
            let mut fitted = size;
            if let Some(adjust) = gui.adjust_size {
                let (mut width, mut height) = (size.width, size.height);
                // SAFETY: the plugin's editor is created, this is its main
                // thread, and the two numbers belong to this function.
                let adjusted = unsafe { adjust(self.plugin, &raw mut width, &raw mut height) };
                if adjusted && width > 0 && height > 0 {
                    fitted = EditorSize::new(width, height);
                }
            }
            let set_size = gui.set_size.ok_or(ClapError::EditorRefused("set_size"))?;
            // SAFETY: the plugin's editor is created, and this is its main
            // thread.
            if !unsafe { set_size(self.plugin, fitted.width, fitted.height) } {
                return Err(ClapError::EditorRefused("set_size"));
            }
            fitted
        } else {
            self.editor_size_now(&gui).unwrap_or(kept)
        };
        if let Some(editor) = &mut self.editor {
            editor.size = applied;
        }
        Ok(applied)
    }

    /// Asks the open editor to draw at `scale`, 2 for twice the size, as
    /// when its window moves to a screen of another density.
    ///
    /// An editor that changes size with its scale asks for its new size
    /// with an [`EditorRequest::Resize`] from the next
    /// [`pump`](Self::pump), and [`editor_size`](Self::editor_size) gives
    /// the size it reports at the new scale. In a Cocoa view this does
    /// nothing, and succeeds: sizes there are in points, which the system
    /// scales, and CLAP asks hosts not to set a scale.
    ///
    /// # Errors
    ///
    /// [`ClapError::InvalidArgument`] for a scale that is not a positive
    /// number, [`ClapError::EditorNotOpen`] if the editor is closed, and
    /// [`ClapError::EditorRefused`] if the plugin does not take the scale,
    /// as one that works its scale out by itself may not.
    pub fn set_editor_scale(&mut self, scale: f64) -> Result<(), ClapError> {
        if !is_scale(scale) {
            return Err(NOT_A_SCALE);
        }
        let (Some(editor), Some(gui)) = (&self.editor, self.extensions.gui) else {
            return Err(ClapError::EditorNotOpen);
        };
        if editor.api == WindowApi::Cocoa {
            return Ok(());
        }
        let set_scale = gui.set_scale.ok_or(ClapError::EditorRefused("set_scale"))?;
        // SAFETY: the plugin's editor is created, and this is its main
        // thread.
        if !unsafe { set_scale(self.plugin, scale) } {
            return Err(ClapError::EditorRefused("set_scale"));
        }
        if let Some(size) = self.editor_size_now(&gui) {
            if let Some(editor) = &mut self.editor {
                editor.size = size;
            }
        }
        Ok(())
    }

    /// Hides and destroys the editor, and stops the timers and descriptors
    /// it registered and left behind. Does nothing if it is closed.
    ///
    /// Close the editor before destroying the window it is in. Dropping the
    /// plugin closes it too.
    pub fn close_editor(&mut self) {
        if self.editor.take().is_none() {
            return;
        }
        if let Some(gui) = self.extensions.gui {
            self.discard_editor(&gui, true);
        }
    }

    /// Destroys a created editor, hiding it first if `hide`, and forgets
    /// what it registered and asked for.
    fn discard_editor(&mut self, gui: &clap_plugin_gui, hide: bool) {
        if hide {
            if let Some(hide) = gui.hide {
                // SAFETY: the plugin's editor is created, and this is its
                // main thread.
                unsafe { hide(self.plugin) };
            }
        }
        if let Some(destroy) = gui.destroy {
            // SAFETY: as above. The editor is destroyed once: the caller
            // has already let go of it.
            unsafe { destroy(self.plugin) };
        }
        // A plugin unregisters what its editor used as the editor is
        // destroyed; whatever it left is taken away now. Requests it made
        // meanwhile are about an editor that is gone.
        self.shared.set_editor_created(false);
        self.shared.editor_loop().forget_editor();
    }

    /// When the next of the plugin's timers is due: the latest moment to
    /// call [`pump`](Self::pump), which fires it. `None` if the plugin has
    /// no timer, or nothing to call when one fires.
    ///
    /// An editor typically draws on such a timer, so an application that
    /// waits for events calls `pump` by this moment for the editor to move.
    /// Timers are not only an editor's: a plugin may register one at any
    /// time.
    pub fn next_editor_timer(&self) -> Option<Instant> {
        self.extensions.timer.and_then(|timer| timer.on_timer)?;
        let editor_loop = self.shared.editor_loop();
        if editor_loop.timers.is_empty() {
            return None;
        }
        editor_loop.timers.next_due()
    }

    /// Fires the timers that are due. For `pump`.
    ///
    /// The ones due are gathered first, and the lock is let go before the
    /// first fires: the plugin may register and unregister timers from
    /// inside `on_timer`. One that an earlier timer of the same round
    /// unregistered does not fire.
    pub(super) fn fire_timers(&mut self) {
        let Some(on_timer) = self.extensions.timer.and_then(|timer| timer.on_timer) else {
            return;
        };
        let mut due = std::mem::take(&mut self.due_timers);
        self.shared
            .editor_loop()
            .timers
            .take_due(Instant::now(), &mut due);
        for &id in &due {
            let registered = self.shared.editor_loop().has_timer(id);
            if registered {
                // SAFETY: the plugin is initialised, this is its main
                // thread, and `id` is a timer it registered and has not
                // unregistered.
                unsafe { on_timer(self.plugin, id) };
            }
        }
        due.clear();
        self.due_timers = due;
    }

    /// The file descriptors the plugin asked to be told about, and what
    /// for, in the order it asked. Watch them with the application's own
    /// loop (`poll`, its toolkit's watches), and call
    /// [`editor_fd_ready`](Self::editor_fd_ready) when one is ready. The
    /// list changes as the plugin registers and unregisters them, so read
    /// it again after each `pump` and each call into the editor. On Windows
    /// no plugin has a use for descriptors, and the list stays empty.
    pub fn editor_fds(&self) -> Vec<FdWatch> {
        self.shared.editor_loop().fds.list().to_vec()
    }

    /// Tells the plugin that the descriptor `fd` is `ready`: has something
    /// to read, can be written to, or has failed. Only what the plugin is
    /// watching `fd` for is passed on; nothing at all for a descriptor it
    /// is not watching.
    pub fn editor_fd_ready(&mut self, fd: i32, ready: FdInterest) {
        let Some(on_fd) = self.extensions.fd.and_then(|fd| fd.on_fd) else {
            return;
        };
        let watched = self.shared.editor_loop().fds.get(fd);
        let Some(interest) = watched else {
            return;
        };
        let happened = FdInterest {
            read: ready.read && interest.read,
            write: ready.write && interest.write,
            error: ready.error && interest.error,
        };
        if happened.is_empty() {
            return;
        }
        // SAFETY: the plugin is initialised, this is its main thread, and
        // `fd` is a descriptor it registered.
        unsafe { on_fd(self.plugin, fd, fd_flags(happened)) };
    }

    /// Turns what the editor asked for since the last `pump` into events,
    /// and destroys an editor that closed by itself. For `pump`.
    pub(super) fn answer_editor(&mut self) {
        let (requests, size) = self.shared.take_editor_requests();
        if requests == 0 || self.editor.is_none() {
            return;
        }
        if requests & editor_request::CLOSED != 0 {
            // Its other requests no longer matter.
            self.editor = None;
            if let Some(gui) = self.extensions.gui {
                self.discard_editor(&gui, false);
            }
            self.waiting.push(HostEvent::Editor(EditorRequest::Closed));
            return;
        }
        if requests & editor_request::RESIZE != 0 {
            self.waiting
                .push(HostEvent::Editor(EditorRequest::Resize(size)));
        }
        if requests & editor_request::SHOW != 0 {
            self.waiting.push(HostEvent::Editor(EditorRequest::Show));
        }
        if requests & editor_request::HIDE != 0 {
            self.waiting.push(HostEvent::Editor(EditorRequest::Hide));
        }
    }
}
