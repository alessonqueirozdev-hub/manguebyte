# manguebyte-au

Builds Audio Units (version 2) for macOS from the Manguebyte plugin API.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

`export_au!(MyPlugin)` exports the same `Plugin` the CLAP, VST 3 and LV2
wrappers take as an Audio Unit, through the plain C AudioUnit interface:
parameters, scheduled parameters and MIDI on their frames, the host's
transport, state and the present preset, the elements' channel counts
from `Plugin::LAYOUTS`, and, for a plugin that sends notes, MIDI 1.0 out
through `kAudioUnitProperty_MIDIOutputCallback`. Each of
`Plugin::AUX_OUTPUTS` is an output element after element 0, named, of
one width; the plugin runs once per slice, whichever element the host
pulls first, and every element is rendered, since a unit cannot switch
one off. `cargo mangue bundle --format au`
writes the `.component` on macOS and `install --format au` puts it where
hosts look. The wrapper's tests act as a host through its C interface on
every system, against a stand-in for CoreFoundation elsewhere than macOS,
and both example plugins pass Apple's `auval` in a macOS CI job. No DAW
has loaded one yet. Not there: an editor, MIDI 2.0 output, factory presets
and AUv3.

The codes come from the plugin's identity at run time
(`manguebyte_plugin::plugin_info`, ADR 0012), in the state dictionary and
in the `Info.plist` that `cargo mangue` writes; a library whose identity
changes with its copy needs its `Info.plist` written for each copy
(`info_plist` takes any `PluginInfo`).

Documentation: <https://docs.rs/manguebyte-au>

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
