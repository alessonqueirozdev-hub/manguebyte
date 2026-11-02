# manguebyte-standalone

Runs a Manguebyte plugin as a standalone application, on the sound card and MIDI ports.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

The plugin runs under a `manguebyte-devices` `DeviceManager`, on the
devices `StandaloneOptions::settings` names: output and input devices by
stable identifier, sample rate, hardware buffer size, and the MIDI input
ports to listen to (all, none or a list). What cannot be had falls back,
and `Standalone::report` says what was opened and what fell back. While
the plugin plays, `Standalone::choices` lists the hosts, the devices with
the rates and buffer sizes each accepts and the MIDI ports, and
`apply_settings` and the `set_` methods change any of it without making
the plugin again: the same instance plays on, with its parameters, state
and editor. Under `RatePolicy::FollowDevice`, the default, a device at
another rate prepares the plugin again at that rate after a short faded
gap; under `RatePolicy::Fixed` the plugin keeps its rate, the device is
resampled at its edge, and a change of device crosses over without a gap
where the system can play both (ADR 0021). `StandaloneSettings` is the
value a program saves with its session, as text, and hands back in the
next run. `Standalone::poll_devices`, from a timer, follows devices and
ports as they come and go. `Standalone::start_on` runs on any device
manager, such as the headless one the tests use. A ready-made settings
screen for all of it is `manguebyte-ui`'s audio settings view, connected
to a `Standalone` by `manguebyte_gui::audio_settings::sync` (feature
`standalone` of `manguebyte-gui`, ADR 0022); this crate depends on no
interface crate.

A plugin that sends notes (`Plugin::NOTE_OUTPUT`) sends them to the MIDI
output port `StandaloneSettings::midi_output` names, as MIDI 1.0 or, with
`midi_output_mpe`, as MPE. The port is opened by the device manager, so
it runs on the headless backend too, follows hot-plug, and changes while
the plugin plays (`Standalone::set_midi_output`, `set_midi_output_mpe`):
the port that is left is sent a note-off for each note it plays and has
its sustain pedals lifted, so no note hangs there, and the audio thread
only queues the notes (ADR 0023).

A plugin's auxiliary output buses (`Plugin::AUX_OUTPUTS`) go to device
channels of their own, by default the ones after the main output's, one
bus after another, for the buses on by default;
`StandaloneOptions::aux_channels` and `Engine::with_aux_channels` move a
bus, mix it into other channels, or leave it unheard, and a bus that is
not heard is not rendered. Ask the device for enough channels with
`StandaloneSettings::output_channels`.

Documentation: <https://docs.rs/manguebyte-standalone>

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
