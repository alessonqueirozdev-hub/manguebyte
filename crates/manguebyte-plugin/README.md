# manguebyte-plugin

The format-independent plugin API of the Manguebyte Audio Framework, with a harness for testing plugins without a host.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

A `Plugin` declares its parameters, its layout and the other layouts it
can run with (`LAYOUTS`, chosen by the host and given to `set_layout`
before `prepare`), the MIDI controllers it listens to, whether it sends
notes, its voices (`voice_info`) and its key names (`NOTE_NAMES`); the
CLAP, VST 3, AU and LV2 wrappers and the standalone runner offer what
their formats can of it. `Harness` runs it in a test, in any of its
layouts, with no host.

A plugin's identity is `Plugin::INFO`, fixed when it is compiled, unless
it overrides `Plugin::info`, which works it out when the library is
loaded: one engine binary copied to several files, each with its own
identity in data it finds through `library_path()`, is then several
plugins. `plugin_info::<P>()` asks `info` once and keeps the answer for
the life of the library; every wrapper, saved state and the harness read
it there. The kind always comes from `INFO`. See ADR 0012.

Documentation: <https://docs.rs/manguebyte-plugin>

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
