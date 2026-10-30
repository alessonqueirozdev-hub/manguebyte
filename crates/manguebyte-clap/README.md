# manguebyte-clap

Builds CLAP plugins from the Manguebyte plugin API.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

`export_clap!(MyPlugin)` exports a `Plugin` as a CLAP with the extensions
`audio-ports`, `note-ports`, `params`, `state`, `latency`, and for a
plugin with an editor `gui`, `timer-support` and `posix-fd-support`; and,
when the plugin declares what they need, per-note automation and
modulation (`ParamFlags::per_note`), `voice-info` (`Plugin::voice_info`),
`note-name` (`Plugin::NOTE_NAMES`), `audio-ports-config` and
`audio-ports-activation` (`Plugin::LAYOUTS`). Each of
`Plugin::AUX_OUTPUTS` is an audio port after the main output, with its
name and width, switched by `audio-ports-activation`; every port starts
active, as CLAP says. Every port takes 64-bit
audio through the plugin's 32-bit path, and transport events inside a
block cut it on their frames. Not there yet: note-end events, `tail`,
presets, remote controls.

`get_info` gives each parameter's current display name. When a plugin
renames parameters (ADR 0020), the host is asked once per batch to read
their information again with `rescan(CLAP_PARAM_RESCAN_INFO)`: on the
main thread after the audio side asks for a callback, at activation, and
together with `VALUES` after a state load or the editor's
`values_changed`.

The descriptor is built from the plugin's identity at run time
(`manguebyte_plugin::plugin_info`, ADR 0012) the first time a host asks
for it, and the path the host passes to `clap_entry.init` becomes
`manguebyte_plugin::library_path()`.

Documentation: <https://docs.rs/manguebyte-clap>

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
