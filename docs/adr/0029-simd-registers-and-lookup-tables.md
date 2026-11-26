# ADR 0029: SIMD registers and lookup tables

- Status: accepted
- Date: 2026-11-26

## Context

JUCE's `juce_dsp` has two things Manguebyte lacked: a SIMD register type
(`SIMDRegister`) with which a processor vectorises its own loops, and lookup
tables. `manguebyte_core::vector` already runs a fixed set of loops on SSE2,
AVX, AVX2, NEON and WebAssembly SIMD, chosen at run time, with the same
results on each. A loop that is not in that set had no way to use them
without `core::arch` and `unsafe` of its own.

Two things make a register type harder than a slice operation. A slice
operation chooses its backend once, inside; a register is used one
operation at a time, so choosing must happen outside the whole loop, and the
loop must be compiled once for each instruction set. And results that
depend on the width of a register differ between processors as soon as a
loop adds lanes together or keeps state per lane.

## Decision

**1. In `manguebyte-core`, with no new dependency.** The register is
`manguebyte_core::simd`, the bottom layer of ADR 0001, on `core::arch` and
the `Backend` of `vector`. `std::simd` is not stable. Crates such as `wide`
choose the instruction set when they are compiled, and `pulp` dispatches at
run time but fixes its own semantics; each would add a dependency to a crate
that has none, for a few hundred lines of one-instruction methods whose
exact results this project has to define and test anyway.

**2. A kernel generic over the instruction set.** A loop is a `Kernel`
whose `run` is generic over `Isa`; `simd::dispatch` calls it from a function
compiled for the processor's best backend, where it is inlined. Every
constructor of a register takes the `Isa`, which only dispatch makes, so
code outside the crate cannot make a register the processor cannot run, and
needs no `unsafe`. The price is that `run`, and what it calls with
registers, must be inlined (`#[inline(always)]`): the compiler does not
require it, and code that is not inlined is correct but, on AVX, slow.

**3. One width on every backend: 256 bits**, 8 `f32` or 4 `f64`. AVX has
it; SSE2, NEON and WebAssembly use two registers, which is also an unroll by
two; the scalar backend uses an array. A loop therefore does the same
operations on the same lanes everywhere, and sums across lanes or state per
lane need no care to be portable. A 128-bit register can be added beside it
if a use asks for one.

**4. Semantics the backends can all give exactly.** IEEE arithmetic and
square root; `add_product` as a product and a sum, with no fused
multiply-add, which SSE2 and WebAssembly lack; `min` and `max` as
`a < b ? a : b` and `a > b ? a : b`, x86's own, built from a comparison on
NEON and from the pseudo-minimum on WebAssembly; reductions in one fixed
order, that of `vector`'s sums. The sign and payload of a NaN made by
arithmetic stay unspecified, as in Rust. While subnormals are flushed
(ADR 0006), x86's `min` and `max` give a subnormal operand back as zero,
where a comparison and a selection keep it; this is documented rather than
prevented, as the compiler is free to turn either form into the other and
the two values count as equal in that mode.

**5. Lookup tables read with the four operations.** `manguebyte_core::lookup`
reads linearly or with a four-point Lagrange cubic, whose error falls with
the fourth power of the spacing and which needs nothing past the ends of a
table (the end intervals use the last four points). Inputs are placed in
`f64`. A NaN or an input outside the range reads an end of the table, so a
table never gives a value that is not finite. The ready-made tables (sine,
`tanh`, `exp`) are filled by the crate's own `f64` functions, not the
system's, so that their bits are the same on every system.

## Consequences

- The tests compare every operation on every backend of a machine with its
  definition in plain Rust, bit for bit; CI runs them on x86-64 (Linux and
  Windows), on AArch64 (Linux and macOS) and, through the
  `simd_register_wasm` example, in WebAssembly with `simd128`. A
  fingerprint of the tables' readings is checked on all of them.
- Adding an operation means writing it for arrays, for pairs of registers
  and for each instruction set, and adding it to the conformance test.
- There are no integer registers, conversions between lanes, shuffles or
  gathers yet, and reading a table is a sample at a time
  ([TECH_DEBT](../TECH_DEBT.md)).
- AVX-512 and fused multiply-add stay out, as in `vector`.
