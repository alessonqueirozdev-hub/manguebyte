# manguebyte-lv2

Builds LV2 plugins from the Manguebyte plugin API.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

`export_lv2!(MyPlugin)` exports a `Plugin` as an LV2 plugin, and
`bundle_files` writes its Turtle files: audio and sidechain ports, one
control port per parameter, MIDI and `time:Position` in through an atom
sequence, state and latency. A plugin that sends notes sends them as
MIDI 1.0 on the `notify` sequence, which then supports `midi:MidiEvent`.
The ports are fixed in the bundle, so a plugin runs its own layout. Each
of `Plugin::AUX_OUTPUTS` is a port group (port-groups extension) of
optional audio outputs after the main ones, one per channel; a bus whose
ports the host leaves unconnected is off. A plugin without buses keeps
the ports it had. No editor. Parameter names are fixed in the bundle too:
hosts show the names the parameters were made with (`ParamInfo::name`),
and a rename while the plugin runs (ADR 0020) is not seen by them.

The descriptor (`plugin_descriptor`, or `descriptor!`) is built on first
use with the URI of the plugin's identity at run time
(`manguebyte_plugin::plugin_info`, ADR 0012), and the Turtle files use the
same; a library whose identity changes with its copy needs its Turtle
written for each copy.

Documentation: <https://docs.rs/manguebyte-lv2>

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
