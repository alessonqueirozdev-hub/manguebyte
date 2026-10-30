# manguebyte-ui

The Manguebyte user interface: a retained widget tree with layout, parameter gestures, accessibility and a test harness.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

Documentation: <https://docs.rs/manguebyte-ui>

## What is in it

- A retained widget tree built from views (`row`, `column`, `stack`,
  `panel`, `card`), laid out by a pure function, drawn on the CPU through
  `manguebyte-gfx` with damage tracking: a steady interface draws nothing,
  and a partial repaint equals a full one bit for bit, shadows that spill
  past a widget included.
- Design tokens (ADR 0011): layered surfaces, three levels of text, one
  accent, categorical hues, semantic roles, elevation, radius, spacing,
  type and motion, in a deep dark theme with glow (the default) and a
  light editorial one, switchable at run time.
- Motion: `Animated` values eased on the frame clock, polled, with no
  timer of their own. Hover, press, focus and selection ease in and out,
  only the widget that moves is drawn again, and nothing moves when the
  theme or the system turns motion off. The focus ring shows after the
  keyboard, not after a click.
- Widgets: `label`, `knob`, `slider`, `toggle`, `pill`, `combo`, `meter`,
  `canvas`, `text_field`, `image` (a picture fitted to its rectangle, with
  a version per density), `scroll_view` (a viewport with scroll bars, the
  wheel, the keys and nested views) and `list_view` (rows the application
  keeps, only those in sight drawn, with selection, the keyboard and
  activation). Popup menus, hover and tooltips.
- Navigation: `tabs` and `sidebar` (with section headers and icons)
  show one page at a time from a `State<usize>`, switched by clicks, the
  arrows, Control+Tab, assistive technology or the application, without
  building anything again. Hidden pages keep their widgets' state and
  cost nothing: they are not polled, laid out, drawn or given frames
  (`Shown`, `UpdateCx::show`).
- `action_button`: primary, secondary and destructive looks with an
  optional `Icon` and a disabled state, its presses counted in a polled
  state. `confirm_dialog` with `IntoView::with_dialog`: a confirmation
  inside the window over a scrim, holding the focus and every key until
  answered (Escape cancels, Enter presses the default), its answer
  polled from a `DialogState`.
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
- Texts that change while the window is open (`TextSource`): labels, the
  names of knobs, sliders, switches and combo boxes (`label_source`) and
  of lists, zone maps, waveform views and keyboards (`list_view_named`
  and the like) can follow a source or a `State`, the language for
  instance, polled once a frame with no heap call unless the source makes
  one. A new name lays out and draws again only what shows it.
- An accessibility description every widget must give, a list of the
  widgets whose description changed in each frame (`Frame::access`) for
  a window layer to pass on, and a test harness (`testing::Harness`) that
  finds widgets only through it.

`examples/views_demo.rs` drives two lists with drag between them inside a
scroll view, with no window, and writes what it drew to PNG files:

```text
cargo run -p manguebyte-ui --example views_demo -- <folder>
```

`examples/sampler_screen.rs` composes the audio widgets into a
sampler's screen of cards (a title bar with pills, a list of samples, the
waveform, a zone map above a keyboard, an envelope and its knobs), draws
it in both themes at 1x, 1.25x and 2x, drives it headless and writes it
all to PNG files; `manguebyte-gui`'s `sampler_window` opens the same
screen in a window on Windows. `examples/design_bench.rs` measures what
the design costs (a whole repaint, an idle frame, a hover transition, a
shadow blurred and cached):

`examples/pages_demo.rs` draws a plugin generator's screen with tabs
(Synth, Effects, Presets) and a confirmation dialog, and an organ
console with a sidebar of divisions, in both themes:

```text
cargo run -p manguebyte-ui --example pages_demo -- <folder>
cargo run -p manguebyte-ui --example sampler_screen -- <folder>
cargo run -p manguebyte-gui --example sampler_window
cargo run -p manguebyte-ui --example design_bench --release
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
