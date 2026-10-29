# manguebyte-core

Core building blocks of the Manguebyte Audio Framework: buffers, samples, smoothing and real-time-safe primitives.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

Documentation: <https://docs.rs/manguebyte-core>

## Vector operations

`manguebyte_core::vector` holds the loops audio code runs most, on slices
of `f32` or `f64`: scaling, adding and mixing (`scale`, `copy_scaled`,
`add`, `add_scaled`, `multiply`), gain ramps (`scale_ramp`), `peak`,
`sum_of_squares`, `dot` products for filters, the complex products of a
frequency-domain convolution (`complex_multiply_add`), stereo
(de)interleaving and the conversions between 16, 24 and 32-bit integers and
`f32`. The `AudioBuffer` helpers, the WAV reader and writer, the
convolution of `manguebyte-dsp` and the mixing of `manguebyte-graph` and
`manguebyte-web` run on them.

Each operation has a scalar definition and versions for SSE2, AVX and AVX2
(chosen at run time, once, with `cpuid`), NEON on AArch64 and WebAssembly's
`simd128` when a module is built with it. `Backend` runs any one of them,
which is how the tests compare them. Nothing allocates, and the crate
still has no dependencies.

The results are the same on every backend and every machine: element-wise
operations and conversions bit for bit with the scalar loop (a product and
then a sum, never a fused multiply-add); sums in one fixed order, 16
running sums for `f32` and 8 for `f64` added in pairs, which is not the
order of adding one element after another but does not depend on the
processor or on where a block starts.

```rust
use manguebyte_core::vector::{self, Backend};

let mut mix = vec![0.0f32; 512];
let voice = vec![0.25f32; 512];
vector::add_scaled(&mut mix, &voice, 0.5);
assert_eq!(vector::peak(&mix), 0.125);
println!("running on {}", Backend::detect().name());
```

`cargo run --release -p manguebyte-core --example vector_speed` times every
operation on every backend the processor has against the loops they
replaced.

## The binary's own file

`manguebyte_core::module::file()` names the file of the binary the code
is linked into, the plugin's library in a plugin, by asking the system
about an address inside it (`GetModuleHandleExW` and
`GetModuleFileNameW` on Windows, `dladdr` on other systems).
`manguebyte_plugin::library_path` is built on it.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
