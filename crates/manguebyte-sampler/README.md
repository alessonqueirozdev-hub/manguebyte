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

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
