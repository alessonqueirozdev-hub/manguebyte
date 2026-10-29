# manguebyte-window

Platform windows for the Manguebyte user interface: child windows for plugin editors, application windows and their loop, file drops and file dialogs, with a headless backend for tests and a Win32 backend.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

Documentation: <https://docs.rs/manguebyte-window>

## What is in it

- `Window`: a child of a host's window for a plugin editor
  (`Window::open`), or an application's own top-level window
  (`App::open_window`) with a title, size limits and a close request the
  application answers. Both present premultiplied pixmaps from a back
  buffer, with damage, timers, input, the clipboard and the system's
  scale.
- `App::run`: the loop of an application's windows, until the last one
  closes or it is asked to quit. Plugins never need it.
- Files and text dragged onto a window (`with_file_drops`,
  `Event::Drop`), through OLE on Win32.
- The system's dialogs to open files, pick a folder and save
  (`open_files`, `pick_folder`, `save_file`).
- A headless backend for all of it on every system, which tests drive and
  script (`Headless`, `HeadlessApp`).
- `system_motion()`: whether the person lets interfaces animate (Windows'
  "Animation effects" setting), which the interface follows (ADR 0011).

Backends: Win32 (ADR 0007 and ADR 0010 say what it does and does not do
to a process) and headless. macOS and Linux are not built yet.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
