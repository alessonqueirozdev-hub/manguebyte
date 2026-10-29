# manguebyte-gui

The glue between Manguebyte's plugin wrappers and its user interface: an editor spec that opens a widget tree in a child window.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

Documentation: <https://docs.rs/manguebyte-gui>

## What is in it

- `Editor`: an editor spec a plugin returns from `Plugin::editor`; the
  CLAP and VST 3 wrappers open it in the host's window, and a test can
  open it headless (feature `headless`).
- Application windows: `Editor::open_in_app` and `open_app_window` show
  an interface in a window of a `manguebyte_window::App`, for a standalone
  program or a plugin run as one, with close requests the application
  answers, files dropped onto widgets and the system's file dialogs
  (ADR 0010). Windows only for now.

`examples/app_window.rs` lists the files dropped on it and has buttons for
the open, folder and save dialogs:

```text
cargo run -p manguebyte-gui --example app_window
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
