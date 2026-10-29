# manguebyte-ui

The Manguebyte user interface: a retained widget tree with layout, parameter gestures, accessibility and a test harness.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

Documentation: <https://docs.rs/manguebyte-ui>

## What is in it

- A retained widget tree built from views (`row`, `column`, `stack`,
  `panel`), laid out by a pure function, drawn on the CPU through
  `manguebyte-gfx` with damage tracking: a steady interface draws nothing,
  and a partial repaint equals a full one bit for bit.
- Widgets: `label`, `knob`, `slider`, `toggle`, `combo`, `meter`,
  `canvas`, `text_field`, `image` (a picture fitted to its rectangle, with
  a version per density), `scroll_view` (a viewport with scroll bars, the
  wheel, the keys and nested views) and `list_view` (rows the application
  keeps, only those in sight drawn, with selection, the keyboard and
  activation). Popup menus, hover and tooltips.
- Audio widgets: `waveform_view` (a sample drawn from a multi-resolution
  `Waveform` summary, with zoom, markers, a selection and a playhead),
  `piano_keyboard` (notes from the pointer and the computer's letters,
  keys lit and ranges marked by the application), `envelope` (an ADSR
  editor bound to four parameters) and `zone_map` (zones of keys by
  velocities, lined up with a keyboard over the same `KeySpan`, made by
  dropping samples). What they change is polled from their states.
- Drag and drop inside the interface (`drag`): typed payloads, targets
  that accept or refuse, a ghost over everything, scrolling near an edge.
  Files and text dragged from other programs reach the same targets
  (`Root::file_drop`), as payloads holding a `DropData`.
- Parameters read from their atomics each frame and changed only through
  gestures; no listeners anywhere: lists, scroll views and text fields
  share their state with the application through values it polls.
- An accessibility description every widget must give, and a test harness
  (`testing::Harness`) that finds widgets only through it.

`examples/views_demo.rs` drives two lists with drag between them inside a
scroll view, with no window, and writes what it drew to PNG files:

```text
cargo run -p manguebyte-ui --example views_demo -- <folder>
```

`examples/sampler_screen.rs` composes the audio widgets into a
sampler's screen (a list of samples, the waveform, a zone map above a
keyboard, an envelope), drives it headless and writes it to PNG files
at scales 2 and 1.25; `manguebyte-gui`'s `sampler_window` opens the same
screen in a window on Windows:

```text
cargo run -p manguebyte-ui --example sampler_screen -- <folder>
cargo run -p manguebyte-gui --example sampler_window
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
