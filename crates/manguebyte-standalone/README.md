# manguebyte-standalone

Runs a Manguebyte plugin as a standalone application, on the sound card and MIDI ports.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

A plugin's auxiliary output buses (`Plugin::AUX_OUTPUTS`) go to device
channels of their own, by default the ones after the main output's, one
bus after another, for the buses on by default;
`StandaloneOptions::aux_channels` and `Engine::with_aux_channels` move a
bus, mix it into other channels, or leave it unheard, and a bus that is
not heard is not rendered. Ask the device for enough channels in
`StandaloneOptions::output`.

Documentation: <https://docs.rs/manguebyte-standalone>

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
