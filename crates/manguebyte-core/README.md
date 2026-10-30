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

## Rendering on several cores

`manguebyte_core::pool::RenderPool` renders independent jobs, such as the
voices of an instrument or the divisions of an organ, on several cores
inside one audio callback ([ADR 0015](../../docs/adr/0015-rendering-on-several-cores.md)).
The pool owns the jobs and a fixed number of worker threads started with
it; each block, `run` renders them on the workers and on the calling
thread, and `rendered` hands them back in index order to mix, so the
output is bit for bit that of one thread whatever the number of cores.

```rust
use manguebyte_core::pool::{Job, PoolConfig, RenderPool};

struct Voice {
    level: f32,
    out: [f32; 128],
}

impl Job for Voice {
    fn render(&mut self) {
        self.out.fill(self.level);
    }
}

let voices = (0..16).map(|_| Voice { level: 0.1, out: [0.0; 128] }).collect();
// Off the audio thread, in `prepare`:
let mut pool = RenderPool::new(voices, PoolConfig::all_cores());
// On the audio thread, each block:
let report = pool.run(16);
let mut mix = [0.0f32; 128];
for (_, voice) in pool.rendered() {
    for (sum, sample) in mix.iter_mut().zip(&voice.out) {
        *sum += sample;
    }
}
assert!(report.is_complete());
```

Nothing on the audio thread allocates or locks, and it never waits longer
than the pool's wait limit: a job a stalled worker still holds when the
limit passes is dropped from that block and counted, never mixed
half-rendered. Workers park between blocks, so an idle pool uses no
processor time, and ask for real-time scheduling (MMCSS "Pro Audio" on
Windows, `SCHED_FIFO` elsewhere), reporting what they got. A pool of zero
workers renders everything on the calling thread.
`cargo run --release -p manguebyte-core --example pool_speed` measures
speed-up, block times and wake latency.
## A budget for the processor

`manguebyte_core::governor::Governor` keeps an instrument that has been
asked for too much inside the block's deadline, by telling it how much
to render. Each block it measures the rendering time against the
deadline and gives a budget for the next block in cost units the
instrument defines (a partial, a voice at a tier); the instrument spends
it in its own order of priority and reports what it spent and whether it
wanted more. The governor learns the cost of a unit on this machine, now,
answers a slowdown within a block (or, if asked, once it has lasted a few),
and recovers by at most a doubling every half second. Offline it always
gives the full budget and never reads the clock; a quality floor and a
maximum bound it. ADR 0018 explains the design.

```rust
use manguebyte_core::governor::{Allowance, Governor, GovernorConfig};

let mut governor = Governor::new(GovernorConfig::default());
governor.set_sample_rate(48_000.0);
// In each audio callback:
let mut allowance = Allowance::new(governor.budget());
governor.begin_block();
for _voice in 0..64 {
    if allowance.try_take(32.0) {
        // Render the voice at its full tier.
    } else if allowance.try_take(8.0) {
        // Render it at its cheap tier.
    } // Otherwise steal it for this block.
}
governor.end_block(128, allowance.spent(), allowance.limited());
```

`cargo run --release -p manguebyte-dsp --example governor_organ` plays a
synthetic organ of `PartialBank` pipes asked for twice what the machine
fits, on the real clock, and prints the loads it measured. On the
development laptop, busy with other programs, on a real-time thread, it
held the load's 99th percentile at the 70 % target with no block late
(ADR 0018).

## The binary's own file

`manguebyte_core::module::file()` names the file of the binary the code
is linked into, the plugin's library in a plugin, by asking the system
about an address inside it (`GetModuleHandleExW` and
`GetModuleFileNameW` on Windows, `dladdr` on other systems).
`manguebyte_plugin::library_path` is built on it.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
