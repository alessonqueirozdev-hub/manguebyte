# manguebyte-sampler

An SFZ sampler for the Manguebyte audio framework: parses SFZ instruments, loads their samples from files or memory and plays them sample-accurately.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

Documentation: <https://docs.rs/manguebyte-sampler>

It plays multisamples with velocity layers, round robins and random
layers, loops with crossfades, release triggers, choke groups and
polyphony limits, an amplitude envelope, a filter per region (`fil_type`
one-, two-, four- and six-pole low-passes and high-passes, two-pole
band-pass and band-reject; `cutoff`, `resonance`, `fil_keytrack`,
`fil_keycenter`, `fil_veltrack`) with its envelope (`fileg_*`), and
modulation by controllers (`<opcode>_onccN` for volume, amplitude, pan,
pitch, cutoff, resonance and every envelope stage, with `set_ccN` and
`set_hdccN` initial values). Controllers run from 0 to 511: MIDI messages
reach 0 to 119, and a plugin sets any of them with
`Sampler::set_controller`, so parameters bound above 127 never collide
with a musician's controllers. The crate documentation lists every opcode
it plays and how; the others are reported as warnings.

Large instruments stream from disk (`Instrument::open_streaming`,
`StreamOptions`): each sample keeps its head and its loops in memory and
the rest is read on reader threads into a lock-free ring per voice, within
a memory budget if one is given. The audio thread never waits for the
disk; a voice whose frames are late fades out without a click and is
counted. What stays in memory can be held packed without loss
(`StreamOptions::compress`), about a quarter of its size for 16-bit
material, and unpacked block by block as voices play it. Players with
voices of their own use the same layer through `SampleStore` and
`StreamCursor`.

For organs: release samples can be chosen by how long the key was held
(`mangue_loheld`, `mangue_hiheld`) and start with an equal-power crossfade
from the sustaining loop, aligned in phase with it
(`mangue_release_crossfade`, `mangue_release_align`); a model outside the
sampler moves each key's pitch and level frame by frame
(`Sampler::modulate_key`), and `SincReader` offers the same windowed-sinc
reading, with a ratio for every frame, to any player.

It plays in any `manguebyte_dsp::tuning::Tuning` too (`Sampler::set_tuning`,
or `Synth::set_tuning` for `SamplerVoice`): temperaments, Scala files and
the MIDI Tuning Standard. A key's tuned pitch takes the place of its number
in a region's pitch, and notes on unmapped keys do not start.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
