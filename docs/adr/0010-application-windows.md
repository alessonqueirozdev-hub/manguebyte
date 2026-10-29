# ADR 0010: Application windows

Status: accepted, 2026-10-29

## Context

ADR 0007 built the window layer for plugin editors: a `Window` is a child
of the window a host gives, it runs on the host's main thread and the
host's loop, and the helpers that make top-level windows and run message
loops exist only behind the crate's `testing` feature, for tests and the
probe. The same ADR asked the layer to "serve the standalone wrapper and
ordinary audio applications as well", and left that for later.

Later is now. The next product built on the framework, Parabolic (a
no-code generator of plugins and samplers, Windows first), is an
application. It needs windows of its own with a title and a size the
person can change within limits, a loop that ends when its last window
closes, a close button it can answer ("save changes?"), files dropped onto
it (a folder of samples), and the system's dialogs to open files, pick a
folder and save. A plugin run as a standalone program needs the first two
as well, to show its editor.

The rules of ADR 0007 still hold, and the hardest of them meet here:

- no hooks, no threads, no mutable statics, no operating-system name that
  is the same in two binaries, never a change to the process's DPI
  awareness, and state in the window class's extra bytes;
- child windows inside a host must keep working exactly as before, in
  hosts that have, or have not, initialised COM and OLE on their main
  thread;
- everything must be testable headless on every CI system, because CI is
  the only place this project can exercise macOS and Linux, and the
  dialogs cannot be clicked in CI at all.

## Decision

**1. Public top-level windows, the same `Window`.** An `App` opens
top-level windows with `App::open_window(AppWindowOptions, handler)`. The
result is the same `Window` type, over the same shared state, damage, back
buffer, pointer, timers, clipboard and reentrancy queue as a child window;
what differs is how it is made and its frame. On Win32 it is a window of
the same per-binary class with an overlapped style, so the class rules of
ADR 0007 apply unchanged. `Window::is_app_window` tells the two apart;
`set_title`, `title` and `size_limits` are for application windows, and
`close` destroys either kind (the handler hears `Event::Closed`, as when a
host destroys a child's parent).

Sizes in `AppWindowOptions` are logical pixels: an application asks for a
600 by 400 window and gets one that looks that size on every monitor. The
window is made hidden at a first guess, asked for its DPI, given the
inside that size needs (`AdjustWindowRectExForDpi`, looked up at run time,
with `AdjustWindowRectEx` as the fallback), and only then shown. The
handler hears nothing of the sizes it goes through. Limits
(`with_min_size`, `with_max_size`) are logical too; they reach the system
in `WM_GETMINMAXINFO` and hold for `Window::resize` as well. A `fixed`
window has no thick frame or maximise box; the program can still resize
it.

**2. The close request is the application's.** The close button, Alt+F4,
the window menu and the taskbar send `WM_CLOSE`, which becomes
`Event::CloseRequested` and nothing else: the window stays open until the
handler calls `close`. A key the handler does not use goes to the
system's default handling rather than to a parent, which is what turns
Alt+F4 into `WM_SYSCOMMAND` and then `WM_CLOSE`, and Alt and F10 into the
window menu. A child window never hears a close request.

**3. The loop.** `App::run` is the thread's message loop
(`GetMessageW`, `TranslateMessage`, `DispatchMessageW`) until no window
the application opened is open (`RunEnd::LastWindowClosed`), `App::quit`
is asked (`RunEnd::Quit`), or someone else posts `WM_QUIT`
(`RunEnd::QuitMessage`). The application keeps weak references to its
windows, so the count is the system's truth after every message, and
`quit` is a flag the loop reads after every message: the framework never
calls `PostQuitMessage`, so it never ends a loop it does not own. A plugin
never makes an `App`: its windows run on the host's loop, as before. An
`App` belongs to its thread; calling `run` from inside a handler is
refused with `WindowError::Busy`.

**4. Monitor changes.** A per-monitor aware application's window gets
`WM_DPICHANGED` when it moves to a monitor of another scale. The scale
changes first (`Event::ScaleChanged`), then the window takes the
rectangle the system suggests, its inside kept within the limits at the
new scale (`Event::Resized`). The glue in `manguebyte-gui` therefore does
not ask for a new size itself, as it does for a child window whose host
gave no scale. The framework still never changes the process's DPI
awareness: an application declares it in its manifest, and a DPI-unaware
one is scaled by the system as any other.

**5. Drops, and OLE in hosts.** A window made `with_file_drops` (child or
application) hears `Event::DragEnter`, `DragOver`, `DragLeave` and
`Drop`, reads the drag's files and text through `drag_data`, and takes or
refuses the drag by its answer, which the system shows with its cursor.
The data is kept in the window from enter to drop, so `Event` stays
`Copy`. A drag's answer is needed before the system's call returns, so a
drag event is never queued for a running handler: the drag is refused
then, and the handler hears of it at the next move. Only a copy, or a link
when nothing else is allowed, is ever answered, never a move, which would
make the source delete what it gave.

On Win32 the drop target is a small `IDropTarget` of the crate's own,
registered with `RegisterDragDrop` when the window is made and revoked in
`WM_DESTROY`; it holds the window weakly. `RegisterDragDrop` needs OLE on
the thread. An `App` owns its thread, so it calls `OleInitialize` when it
is made and `OleUninitialize` when it and every one of its windows are
gone (the windows keep it alive). A child window never initialises OLE:
the thread is the host's, and changing its apartment could break the
host. Where the host has OLE, a child window that asks for drops takes
them; where it has not, it takes none, and `accepts_drops` says so. Drops
are off by default for child windows, because a child that takes drops
hides whatever drop target the host has under it.

Found while testing: on a thread with no COM of its own, while another
thread of the process has OLE, this system's `RegisterDragDrop` fails and
still marks the window as registered. The window therefore registers only
when `CoGetApartmentType` says its thread is a single-threaded apartment,
and revokes after any failed registration.

**6. Drops reach widgets through ADR 0009's drag targets.** ADR 0009 gave
drags inside the interface a payload of the application's type and an
optional source, so that drags from outside could use the same targets.
`Root::file_drop(FileDrop, &DropData)` does that: a drag from another
program becomes a drag of the interface's own with no source and no ghost
(the system draws its own image), whose payload holds the `DropData`.
Widgets take files through `Widget::drag` like anything else dragged, and
a scroll view near whose edge the pointer rests scrolls the same way.

**7. Dialogs.** `open_files`, `pick_folder` and `save_file` take a
`FileDialog` and return `Option<Vec<PathBuf>>` or `Option<PathBuf>`
(inside a `Result` for a dialog the system could not show); `None` is a
cancel. They are `IFileOpenDialog` and `IFileSaveDialog`, modal to the
window's top-level window, returning file-system paths only and leaving
the process's current folder alone. The dialog runs a modal loop of its
own; the window keeps answering paints from its back buffer meanwhile,
and events for a handler that is the dialog's caller wait for it, merged
as ADR 0007's dispatcher merges them. A second dialog of the same window
is refused while one is open. The dialogs are COM objects: on a thread
with no COM, COM is initialised for the length of the call only. A
`Dialogs` handle, which holds the window weakly, lets an interface's
buttons show them.

**8. What the headless backend scripts.** `App::headless` makes headless
windows and a loop of steps a test posts (`HeadlessApp::post`) in place
of messages; `run` returns `RunEnd::Idle` when the steps run out with
windows open, where a system loop would wait. The `Headless` controller
plays the system: `request_close`, `drag_frame_to` (within the limits, not
at all for a fixed window), `move_to_monitor` (the scale, then the
suggested size), `drag_enter`, `drag_over`, `drop_at`, `drag_leave`, and
for dialogs `answer_next_dialog` and `on_next_dialog`, whose answer runs
while the dialog is "open" and may drive the window, and `take_dialogs`,
which records what was asked. The Win32 tests make real application
windows, read their pixels back, run the real loop, deliver drops through
the registered target with a data object of their own, and make and
configure the dialogs through COM without showing them. Showing a dialog,
and a drag from the file manager through OLE's own loop, are tested by
hand only.

## Consequences

- An application, or a plugin run as one, can be built on the framework
  on Windows today, and its interface tested end to end without a screen
  on every CI system.
- The public API of `manguebyte-window` grows by a type for applications,
  options, four events and a dialog description; `Event`, `WindowOptions`
  and `WindowError` were already non-exhaustive, so nothing that compiled
  breaks.
- The crate now implements one COM interface and calls a handful of
  others, declared by hand in `win32::com` because `windows-sys` has the
  functions and structures but not the interfaces' tables. Every method
  table is in the SDK's order, and only the methods called are typed.
- macOS and Linux have no application windows yet; `App::new` is
  `Unsupported` there, and only the headless backend runs. TECH_DEBT item
  97 lists what else is left.

## Alternatives not taken

- **A window type of its own for applications.** Two types would duplicate
  every request and the handler's view of them, for a difference of how a
  window is made.
- **A loop that runs whenever any window exists, child windows
  included.** A plugin's windows run on the host's loop; a framework loop
  in a host would take over the host's messages.
- **`WM_DROPFILES` (`DragAcceptFiles`) instead of OLE.** It needs no OLE,
  but tells the window nothing until the drop: no position while dragging,
  no way to refuse, no cursor feedback, and no text.
- **Initialising OLE for a child window when the host has not.** OLE is
  per thread; an apartment the framework chose for the host's thread could
  break the host's own COM later.
- **The `windows` crate for COM.** It would bring its implementation
  macros and their dependencies for one small interface; the
  declarations needed are a few dozen lines.
