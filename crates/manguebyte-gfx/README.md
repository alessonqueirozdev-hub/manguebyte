# manguebyte-gfx

2D graphics for the Manguebyte user interface: scenes, a rasteriser behind a seam, text with an embedded typeface.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

Documentation: <https://docs.rs/manguebyte-gfx>

## What is in it

- Scenes: fills and strokes of shapes and paths with solid colours and
  linear and radial gradients, images, text, clips, transforms, layers,
  columns for waveforms and meters, and soft shadows and glows of any
  filled shape or stroked outline (`Scene::shadow`,
  `Scene::stroke_shadow`). `Scene::dump` prints a scene exactly.
- `Renderer`: draws a scene on the CPU at any scale, whole or only inside
  damage rectangles, a partial render equal to a whole one bit for bit.
  It keeps glyph masks and blurred shadow masks in bounded caches, so a
  shadow is blurred once (three box passes on whole numbers, the same on
  every machine) and blended from the cache after (ADR 0011).
- An embedded subset of Inter, shaped and laid out (ADR 0007).

`examples/render_bench.rs` measures the renderer; `manguebyte-ui`'s
`design_bench` measures a whole screen of cards and shadows.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
