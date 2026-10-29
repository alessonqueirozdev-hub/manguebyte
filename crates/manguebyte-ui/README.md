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
- Drag and drop inside the interface (`drag`): typed payloads, targets
  that accept or refuse, a ghost over everything, scrolling near an edge.
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

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
