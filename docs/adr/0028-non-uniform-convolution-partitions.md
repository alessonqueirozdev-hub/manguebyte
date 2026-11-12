# ADR 0028: Non-uniform convolution partitions

Status: accepted, 2026-11-12

## Context

`ConvolutionEngine` convolves in partitions of one size: a response of `n`
taps costs `n / b` products of spectra of `b`-sample partitions per channel
path, in every period of `b` frames. The size `b` is the caller's, and the
deadline sets it: a host that asks for blocks of 64 frames wants small
partitions, so that the direct head stays short and the transforms stay
small. A three-second stereo response at 48 kHz in blocks of 64 then costs
about 2 250 products of 130-point spectra every 64 frames — which on a
12th-generation Intel Core i3-1215U runs slower than the audio arrives
(`examples/convolution_speed.rs` measures 0.1 to 0.6 of real time), and a
large share of the cost is the bookkeeping of thousands of tiny partitions,
not the convolution itself.

The standard way out is a partition size that grows with the tap position:
the beginning of the response, where the deadline is, stays in small
partitions; the tail, whose contribution to the current output was known
whole partitions ago, convolves in big ones, few enough that the total is
cheap. JUCE's convolution does this with two sizes; the scheme here is a
ladder of them.

The constraints are the convolution's own: nothing on the audio thread
allocates, locks, waits or frees (ADR 0002, ADR 0003); the output is the
same bit for bit however the host splits its blocks; zero-latency mode has
the response's first sample out at the first frame; responses swap with a
crossfade while playing; and every band still has to hold one rule — a
transform is only ever run on input that has already arrived.

## Decision

**1. Bands of doubling sizes after the first partition.** The scheme is
chosen by `ConvolutionSettings::with_non_uniform_partitions(largest)`. With
`largest` equal to the first partition size `b` the engine is the uniform
one, unchanged; `with_partition_size` alone keeps it so. With a larger
`largest` — a power of two at least `2 * b`, refused otherwise — the first
partition size covers exactly one partition of the response after the head,
and bands of partitions of `2 * b`, `4 * b`, `8 * b`, up to `largest` cover
the rest: each middle band exactly one partition, the largest band as many
as the tail needs. A 144 000-tap response at `b` = 64, `largest` = 8 192 is
24 products a period against 2 249.

A band is the uniform machinery at its own size: an input window of two
partitions per channel, a ring of input spectra, one accumulator and one
pending buffer of a partition per output channel, and the same spread of
the older partitions' products over the filling one (`sum_ahead`, so the
order of the additions is the same whatever the blocks).

**2. The layout is `off = size - delta`, so the band boundaries can
stagger.** A band of size `s` covers the response from tap `s - delta`,
where `delta` is `block - head_len`: zero in zero-latency mode, where the
direct head plays taps 0 to `b - 1` and every band's first partition starts
at its own size; `b` in partition mode, where every band's output is the
same `b` frames late as the smallest partitions', so `latency()` stays `0`
or `b` whichever the scheme. A band's partitions never overlap the next
band's: a middle band covers `[s - delta, 2 s - delta)`, and the last one
runs to the response's end.

**3. Each band's boundaries land half a period after every smaller
band's.** A band starts — from construction and from every `clear` — with
its partition half full, so its boundaries fall at `s / 2` modulo `s`,
which is a multiple of `b` and so always inside a `b`-sized piece of the
processing loop, and never at another band's boundary (size `2 s` bounds
at `s` modulo `2 s`, which is `s / 2` modulo `s` shifted by `s / 2`). One
band does its boundary work in one `b`-sized piece at most; two bands
never share a piece for their boundary work, and the class of the smallest
partitions does its own transform at every boundary as before.

**4. The inverse transform and the newest partition's product are owed to
the next piece (`inverse_due`).** At a boundary the band finishes the
spread of the older partitions' products, runs the forward transform and
puts the spectrum away; the product of that spectrum with the band's first
partition, the inverse transform and the pending copy run at the start of
the next `b`-sized piece, before any output is read from the band's
pending. So the two transforms of the band's size never land in one piece,
and the piece after a boundary does one inverse of `s` against the
boundary piece's forward of `s` — the spread the staggering pays for. The
band's pending is written only there, so output keeps reading the previous
partition's pending until the inverse has run: each band plays a partition
late, exactly like the smallest partitions' own pending, and the `off`
layout absorbs it into `delta`.

Everything else is unchanged: `process` still splits host blocks at the
smallest partition's boundaries, so block sizes 1 to 4 096 and irregular
patterns give the same bits; the `Convolution` wrapper's crossfades take
whole engines, scheme included; a response shorter than a band's first
tap skips that band, so a non-uniform setting degrades gracefully to a
smaller scheme and, for a response under `2 * b`, to the uniform one.

## Alternatives

- **Two sizes, as JUCE does it.** Cheaper to describe and to schedule, but
  for a fixed first size it either leaves the tail in too many partitions
  of the second, or makes the second so large that its boundary block
  costs more than the deadline. The ladder costs a few lines more and buys
  a size that keeps growing where the response does.
- **Spreading the large transforms over the audio thread's own blocks.**
  The forward transform of `s` samples could run `b` samples at a time over
  `s / b` pieces. It would flatten the peak further, but it makes the
  transform's own ordering depend on the host's blocks — which the
  bit-for-bit promise forbids — and it keeps a half-done transform alive
  across pieces, one more piece of state to reason about; the staggering
  already keeps the worst block at one forward of `s` plus one inverse of
  `s` plus the small partition's own work.
- **A lock-free worker for the large bands.** An idle core could take the
  bands' transforms, with the audio thread polling for the result, as the
  render pool does for jobs (ADR 0015). It is the JUCE answer, and it buys
  a second core; it also buys a thread's worth of state, a polling
  contract per band, and a fallback path for when the worker is late — a
  lot to keep honest on every platform for a problem the ladder already
  halves twice over on this machine (0.1–0.6 of real time made 1.7–4.1).
  The engine keeps the option open: bands depend on no state outside
  themselves, so a worker could take one later without the scheme changing.

## Consequences

`ConvolutionSettings` gains a `largest_partition_size` (constructor
argument through `with_non_uniform_partitions`), and `ConvolutionEngine` a
`largest_partition_size()` read and a `bands` field. The uniform path is
untouched: an engine built the old way is the old engine, and its tests
run on it unchanged. For a response of length `r`, memory grows to about
`sum of 4 s_i + s_i log-ish` per channel — the same order the uniform
scheme already pays — while the product count drops from `r / b` to about
`log2(largest / b) + (r - largest) / largest`.

The cost model a caller should keep in mind: the largest band's boundary
block does one forward and one inverse transform of `2 * largest` points,
spread as one per piece, so `largest` is limited by the deadline's worst
block, not by the mean load; for a long tail at small `b` it is still the
mean that dominates, and the ratio of them is the point of the scheme.

Verified: output equals the uniform scheme's and direct convolution's to
the measured error (within 2e-6 of the RMS for `f32`, 1e-13 for `f64`),
bit for bit across host block sizes 1 to 4 096 and irregular patterns;
zero-latency mode emits the response's first tap at the first frame; a
crossfade between two non-uniform engines mixes their own outputs; a NaN
in the input washes out of every band's windows and rings; the allocation
guard watches non-uniform engines process and being swapped, in both
routings, and sees no heap call.
