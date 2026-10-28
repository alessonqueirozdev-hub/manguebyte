# manguebyte-host

Plugin hosting for the Manguebyte Audio Framework: load CLAP, VST 3, LV2 and Audio Unit plugins and run them as processors.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

It loads plugins made by anyone and runs them as processors, through one
interface for all four formats (`any`), with every event on its frame and
nothing allocated while processing. The thread rules are types: the
main-thread half of a CLAP or VST 3 plugin or an Audio Unit cannot be sent
to another thread. LV2 bundles are read with the crate's own Turtle
reader, which runs no code.

Audio Units (version 2, feature `au`) are found in the system's registry
(`AudioComponentFindNext`) and made through AudioToolbox on macOS, or made
in process from a component's factory function on any system. The host
sets their stream formats, longest slice and channels, feeds the main
input and the sidechain through render callbacks, schedules parameter
changes (`AudioUnitScheduleParameters`) and MIDI (`MusicDeviceMIDIEvent`)
on their frames, answers the transport callbacks for each slice, reads
latency and tail, keeps state as the XML of `ClassInfo`, and bypasses.
The framework's example plugins hosted as Audio Units sound the same, bit
for bit, as with no host; through AudioToolbox that is checked by the
macOS job of continuous integration, against the installed components.

`scan` finds the plugins installed on a system and lists each CLAP and
VST 3 file, and each unit of an AU bundle, in a process of its own with a
time limit, so one that crashes or hangs is reported and the host goes
on, with a cache of what it found.

The editors of CLAP and VST 3 plugins open inside a window the
application owns, through the same methods for both formats: open in a
parent window, resize, rescale, close. What an editor asks for (a new
size, to be shown or hidden) comes back from `pump`, which also runs its
timers, and an edit made in it reaches the hosted processor. This is
tested in real Win32 windows; on macOS and Linux no editor has been
opened in a real window yet.

Not yet: the editors of LV2 and AU plugins, reading the notes and MIDI a
hosted plugin sends, Audio Units version 3 as such, and runs against
plugins from other vendors.

Documentation: <https://docs.rs/manguebyte-host>

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
