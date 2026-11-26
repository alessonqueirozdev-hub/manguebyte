# ADR 0028: Windows on macOS and X11

- Status: accepted
- Date: 2026-11-24
- Builds on: [ADR 0007](0007-user-interface-layer.md) (decision 4),
  [ADR 0010](0010-application-windows.md)

## Context

`manguebyte-window` has two backends: Win32, and a headless one for tests.
Plugin editors and application windows therefore open on Windows only, which
is the largest gap left against JUCE's `juce_gui_basics` (see
[JUCE_PARITY](../JUCE_PARITY.md)) and keeps every Manguebyte instrument, such
as Caecilia, from shipping complete on macOS and Linux.

ADR 0007 already chose the shape: a thin backend per platform behind the
crate-private enum, presenting with the platform's plain copy (layer contents
on macOS, `PutImage` on X11), the interface on the host's main thread, and on
Linux through the host's loop (CLAP `posix-fd-support` and `timer-support`,
VST 3 `IRunLoop`). It left two things open: how to reach the systems, and
whether CI can read a window back on macOS and Linux. This ADR answers the
first and plans how to answer the second.

The work is driven entirely through GitHub: every part is proved by the CI
runners (`macos-latest`, `ubuntu-latest`, `ubuntu-24.04-arm`), not by a
local machine.

## Decision

**1. macOS through `objc2-app-kit`.** The macOS backend calls AppKit and
Core Animation through `objc2-app-kit` and `objc2-quartz-core` (0.3, MIT),
the same family as the `objc2` 0.6, `objc2-foundation` 0.3 and `block2` 0.6
already in `Cargo.lock`. They are generated from Apple's headers, as
`windows-sys` is from Windows', and are compiled on macOS only, with only the
features used. The view's Objective-C class is declared once per binary with
a name derived from the binary, never a fixed one, so that two plugin
binaries in one process do not collide (the fault ADR 0007 found in other
toolkits).

**2. X11 through `x11rb` with its pure-Rust connection.** The Linux backend
speaks the X11 protocol through `x11rb` (MIT or Apache-2.0) and its
`RustConnection`, so it needs neither `libX11` nor `libxcb` to build or run,
compiled on Linux only, with only the extensions used. Wayland is not part of
this ADR: a plugin editor on Wayland still embeds through XWayland today, and
application windows on Wayland are a later decision.

**3. The same seam.** Each backend is a new arm of the crate-private enum, and
runs the same damage merging, back buffer, pixel conversion, pointer capture,
click counting, timers, caret and dispatch queue as Win32 and headless. Only
the system calls differ. The crate keeps `#![deny(unsafe_code)]` outside the
platform modules, and each `unsafe` block in them says why it is sound.

**4. Hygiene, as on Win32.** No mutable statics, no threads, no hooks, no
process-wide settings. On X11 the window never runs its own event thread:
a child window is driven by the host's loop through the file descriptor and
timers the editor contract already offers, and an editor refuses to open
where the host offers neither. On macOS everything happens on the main
thread, which is where hosts call editors.

**5. Proof by reading back.** A part is done only when a test on the CI
runner opens the real window, presents a known picture, reads the pixels
back from the system and compares them, and injects input and sees it
arrive: on X11 under `Xvfb` with `GetImage` and the XTEST extension; on
macOS by asking the view to draw into a bitmap (no screen-recording
permission needed) and by sending `NSEvent`s to it. If the macOS runner cannot
show a window at all, the macOS part stops there, and the fallback of ADR
0007 (adapting `baseview`'s view code, MIT or Apache-2.0) is taken up.

## Parts, in order

Each part is a pull request of its own, merged only with every CI job green.

1. **X11 child windows**: a window inside a host's X window (`Window::open`
   with an X11 parent), presenting with `PutImage`, damage, expose, resize,
   pointer, wheel and keys (with the keyboard map), focus, the scale from
   `Xft.dpi`, timers and the descriptor through the editor host; a
   `Window probe (X11)` job under `Xvfb`.
2. **macOS child windows**: an `NSView` inside a host's view, presenting as
   layer contents, the same events, the backing scale factor; a
   `Window probe (macOS)` job.
3. **Editors in hosts on both**: CLAP `gui` with `CLAP_WINDOW_API_X11` and
   `CLAP_WINDOW_API_COCOA`, VST 3 `IPlugView` with `kPlatformTypeX11EmbedWindowID`
   and `kPlatformTypeNSView` (with `IRunLoop` on Linux); the example plugins'
   editors opened through both formats' real interfaces in CI, and hosted
   editors in `manguebyte-host` on both systems.
4. **Application windows on X11**: top-level windows, the application's loop
   (`App::run`) and its timers, close requests, size limits, the clipboard
   (selections), drops (XDND).
5. **Application windows on macOS**: `NSWindow`, `NSApplication`'s loop and
   timers, the pasteboard, drops, `NSOpenPanel` and `NSSavePanel`.
6. **File dialogs on Linux**: through the XDG desktop portal over D-Bus,
   which is a decision of its own (a D-Bus client, or none) when this part
   starts.

Platform accessibility (AccessKit or our own adapters) stays out, as ADR 0007
decision 6 says: each adapter comes after a test with two plugin binaries in
one process on that platform.

## As built

**Part 1, X11 child windows.** A window in an X11 parent needs the host's
loop, which `Window::open` has no way to take, so it is made with
`Window::open_with_context`, given the editor's `EditorContext`; `open`
answers `WindowError::NoHostLoop` for an X11 parent. The context offers
neither or either of a descriptor watch and timers: with no watch, a 16 ms
timer reads the connection; with no timers, the window's own timers are
refused; with neither, no window opens. One extension is used, XKB, so that
a held key's repeats are told apart and a new keyboard map is heard of. The
window has to itself a connection to the server `DISPLAY` names.

## Consequences

- Two dependencies arrive, each on one platform only, and each from a family
  already trusted: `objc2` is in the lock file; `x11rb` is new.
- macOS and Linux claims in [JUCE_PARITY](../JUCE_PARITY.md) and the README
  wait for each part's tests, as Windows' did.
- Wayland-only sessions without XWayland get no window yet.

## Alternatives not taken

- **`winit` or `baseview` as the backend.** ADR 0007 explains why: fixed class
  names, threads of their own and process-wide state, which two plugin
  binaries in one process cannot share. `baseview`'s macOS view code stays
  the fallback for part 2 only.
- **Hand-written declarations on macOS.** Fewer dependencies, but more code of
  ours and more chances of a wrong signature in `objc_msgSend`, which no
  compiler checks.
- **`libxcb` through the system.** No new crate, but the library must be
  installed to build and test, and every call would be our own `unsafe`.
