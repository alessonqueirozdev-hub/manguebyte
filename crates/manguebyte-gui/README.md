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
- Zoom to fit: `Editor::with_zoom_to_fit(min_zoom)` keeps the layout at
  the editor's design size and draws it as large as the window allows,
  with bars in the theme's colour around it, in a host and in an
  application's window alike. Clicks, assistive technology's bounds and
  the caret follow the zoom; below `min_zoom` the layout reflows, and
  `Editor::with_real_size_below` gives a compact layout at its real size
  below a breakpoint, with `manguebyte_ui::responsive` (ADR 0027).
- With the `standalone` feature, `audio_settings`: `sync` connects
  `manguebyte-ui`'s audio settings view to a running
  `manguebyte_standalone::Standalone`, carrying out what the person picks,
  following devices that come and go and showing the report of each
  change (ADR 0022). Off by default, since it brings the system's audio
  and MIDI libraries.
- Frames run from one window timer, about 60 a second; a frame where
  nothing changed presents nothing, and motion follows the system's
  setting in real windows (ADR 0011).

`examples/app_window.rs` lists the files dropped on it and has buttons for
the open, folder and save dialogs:

```text
cargo run -p manguebyte-gui --example app_window
```

`examples/zoom_responsive.rs` is an organ console designed at one size, in
a window whose border can be dragged: it zooms down to a breakpoint, and
below it gives way to a miniature of each manual at its real size:

```text
cargo run -p manguebyte-gui --example zoom_responsive
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
