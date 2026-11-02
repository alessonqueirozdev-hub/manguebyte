# ADR 0026: Voicing a partial

Status: accepted, 2026-11-02

## Context

Caecilia, a pipe organ built on Manguebyte, renders each pipe as a
`PartialBank`: quadrature phasors, one per partial, under gains and
rotations it sets every control block. The person who plays it says it
sounds extremely artificial, kind of canned. Its partials are perfectly clean
sines with a slow drift. What a real pipe has and these do not: turbulence
around each partial, partials that do not all start together, and wind that
never holds perfectly still.

The constraints: thousands of partials sound at once, in a kernel that
gets its speed from having nothing to do but turn phasors; nothing on the
audio thread allocates, locks or waits (ADR 0002, ADR 0003); the bank promises
the same bits on every backend and whatever the blocks, and promises that a
partial follows Caecilia's scalar kernel bit for bit; and Caecilia's bellows
model already calls `set_rotation` every control block, so anything else that
moves a frequency has to add to it and not fight it.

## Decision

**1. A complex modulator on each partial, on the way to the output.** The
kernel gains a sibling, `Backend::add_modulated_partials`, which works out
`gain * (re * sin + im * cos)` where the plain one works out `gain * sin`:
the imaginary part of `(re + i im)(cos + i sin)`. The modulator's magnitude
scales the partial and its angle moves its phase. Phasors and gains are left
to move exactly as they did, bit for bit, so a phasor is still only a phasor,
`gain` and `ramp_to` mean what they meant, and the one number that decides
a partial's output is the product of what the caller set and what the
voicing adds. Both kernels are one function behind a const parameter, so the
order of the sums has one definition and the plain kernel's AVX2 code came out
the same, instruction for instruction, as before this change.

Everything multiplies: air, the attack's level and the wind's gain all go
into that one modulator:

    m = wind · (level + (level · air + breath) · n)

**2. Air is `1 + depth · n(t)`, for complex noise `n` of a partial's own.**
The sine grows a skirt, and its amplitude and phase wander a little and
come back. `n` has a mean of 0, so the line stays where it was, and a mean
power of 1, so the skirt's total power is `depth²` times the partial's. It
comes from a complex one-pole low-pass over white noise, whose power falls as
`1 / (1 + (2 f / B)²)`: this is the spectrum a damped resonator driven by
noise has, which is what turbulence driving a pipe's resonance is, so the
one-pole is not only the cheap choice. Its pole is worked out so that its half
power is exactly at `B / 2` (not the usual `exp(-w)`, which drifts from that
towards the upper end of the range), and its input is scaled so that the power
stays at 1 at every bandwidth, including what the straight lines between values
take off.

**3. An update every 32 frames, and straight lines between.** At 48 kHz that
is 1 500 Hz, against bandwidths of a few hertz to a few hundred. Noise at that
rate has copies of its spectrum around multiples of it, which the straight
lines bring down by `sinc⁴`: measured, the first image is 62 dB below the
skirt's peak for a skirt of 40 Hz, itself some 20 dB and more under the
partial. An update of 16 frames would cost twice as much to take that image
down further under something already inaudible; one of 64 would cost half, and
leave the top of the range at 375 Hz, narrower than a chiff wants, with the
images 12 dB nearer. 32 divides `NORMALISE_FRAMES`, so the renders split in no
new places on one of those frames. The rate follows the sample rate; it is
frames that are fixed, and bandwidths in hertz are worked out for it.

**4. A per-partial generator, stepped only on updates.** One xorshift32 a
partial, whose high and low halves give the two parts of `n`, seeded from one
seed by SplitMix64. A generator moves only on updates when its partial has air
or breath, so a partial's stream depends on its own settings and on no other
partial's, whenever those start. Generators never restart on a note, only on a
new seed, so that one note never repeats another. The update is plain Rust over
slices the compiler knows are apart, which vectorises on the baseline
instruction set and in exact IEEE arithmetic gives the same bits as any other
compilation of it, so the bank's backends do not enter into it. No platform
maths is used: arctangent, cosine and square roots come from series or from
`f32::sqrt`, so the render is reproducible on any machine given rotations that
are (`set_frequency` uses the platform's sine, as it always did).

**5. An attack is a formula, not a state machine.** Level, breath, pitch and
pole come out of a branchless formula of a partial's age, evaluated for every
partial on every update while any attack runs, a vectorised loop like the
noise. A slot with nothing in it gives a level of exactly 1, no breath and no
shift, so no partial needs a test. A first version, with a structure per attack
and a branch per stage, cost about twice as much on top of the air.

The level is sampled at the updates and joined by straight lines, so it is
right within what a line of 32 frames misses of a smoothstep, not at every
frame exactly. A delay is then not rounded to an update: the samples at the
updates still differ for delays that differ by a few frames.

The breath does not wait for the delay or the rise, since in a real pipe the
air arrives before the tone: `(level · air + breath) n` is not scaled by the
level. It starts wide and narrows, by moving the noise's pole, as the pipe's
resonance builds up. The pole moves between two poles, the breath's and the
partial's own as it is now, so that a bandwidth or a sample rate changed during
the attack cannot take it outside the unit circle. The breath counts from the
first update, not from the call, because a fall that short is what the updates
read of it: counted from the call, one of 64 frames differed by 60 dB by where
it landed, and one of 20 was not heard at all.

An attack from silence drops the level to 0 at once, which clicks on a partial
that is sounding, so `restart_attack` starts one from the modulator as it is
now: the level holds through the delay and rises from there, for a key struck
again during a release, which keeps its gain and phasor.

**6. Wind jitter is one wander through both gain and rotation.** Two one-pole
low-passes in a row over white noise, one generator a bank, normalised to a
standard deviation of 1 and scaled by a depth in cents and one in decibels.
One wander, because one wind moves both. The normalising scale is on the
filter's input, so that the filter's output has that standard deviation at
every rate, and a new rate neither moves the wander nor deepens it.

The bank keeps what the caller set (`cos_step`, `sin_step`, returned by
`rotation`) apart from what the kernel turns by, and each update works the
second out from the first: a rotation of angle `w` becomes `w (1 + shift)`,
by turning it through `w · shift`, a small angle whose sine and cosine are
series. The ratio is the same for every partial, so harmonics stay harmonic.
A caller's rotation goes through at the next update when a shift is in force,
and at once when not. A wind model's cents and the jitter's therefore add
exactly, with no communication between them. `w` itself comes from an
arctangent series from the rotation, not asked of the caller, so that
`set_rotation` stays two stores.

**7. A bank that asks for nothing pays for nothing.** `set_air` with a depth
of 0, a default attack and `WindJitter::OFF` touch nothing. Until something
is on, the render runs the old kernel; when everything has ended, the
modulators are given one more update to come home and it goes back. The bits
are those of before this change, on the fingerprints taken from it.

**8. `PipeVoicing::FLUE_PRINCIPAL`** gathers a starting point, with what the
literature gives of it and what is a judgement said in its comments: nearly
all of its numbers are judgements, and no one has heard them on Caecilia yet.

## Consequences

- With everything off: the same bits, by test, and no measurable cost; the
  voicing's arrays are there all the same, 140 bytes a partial on top of the
  bank's own 40.
- 2 000 partials at 48 kHz in blocks of 128 frames, on one logical processor of
  an Intel Core i3-1215U (`examples/partial_voicing_speed.rs`), against the
  plain kernel, across two runs and whether as one bank or as 125 banks of 16:
  air alone 1.4 to 1.6 times (0.05 to 0.11 of a core on AVX2); air and jitter
  1.6 to 2.1 (0.07 to 0.13); all three with every partial always in an attack
  1.8 to 2.5 (0.08 to 0.15). With other work on the machine earlier runs went up
  to 2.9. Attacks last a moment and are rarely all at once, so the last is a
  peak and not a sustained load.
- What is promised exactly: the same bits however the caller cuts its blocks,
  given settings on the same frames; the same bits on the backends run (scalar,
  SSE2, AVX and AVX2, on one x86-64 machine; NEON and WebAssembly SIMD share the
  generic kernel's code and have not been run); the same bits in a debug and an
  optimised build. Given rotations that do not use a platform's maths, the
  update has none in it either, so another machine should agree, and the
  fingerprints in `tests/partial_voicing.rs` will say so when one runs them.
- A new rotation lags up to 31 frames while a shift is in force.
- With any partial voiced, the whole bank runs the dearer kernel.
- The caller must give each pipe its own seed; two banks with the same one
  have the same noise on the same partials.
- A release needed nothing: `ramp_to` already has a length per partial, and
  Caecilia's exponential, darkening release is a matter of the gains it sets.

## Alternatives not taken

- **Additive noise, a second oscillator per partial with a noisy amplitude.**
  Twice the phasors, for what a modulator does in five more operations a frame.
- **A resonator per partial driven by noise at the audio rate.** A truer
  model, with a filter recursion per partial per frame, which is the cost this
  bank exists to avoid; the one-pole at baseband has the same line shape.
- **A complex gain instead of a separate gain and modulator** (`gr += ..`,
  `gi += ..`, `v = gr * y + gi * x`): three more operations a frame in place
  of five, but the caller's ramps would have to be folded into it at every
  update, with `gain()` no longer the running value, and the bit-for-bit
  identity of gains with the plain kernel lost.
- **Applying the jitter as a phase modulation through the modulator**, which
  would leave the rotations alone: a wander of frequency is a random walk of
  phase, without bound, so it needs a high-pass and stops being a depth in
  cents; and it needs `w` all the same.
- **A wind model for the bank** in place of a wander. The bellows are
  Caecilia's own model; this is the unsteadiness left around it.
- **Gaussian noise.** Uniform noise is cheaper, and the filter's sum is
  Gaussian enough at any bandwidth wanted here.
- **Vectorising the update through the backends.** It runs once in 32 frames,
  so it matters less than the kernel, and the compiler's baseline SSE2 was
  enough to bring it down; a backend's AVX2 version is left for when a profile
  asks (TECH_DEBT item 119).
- **An update of a jitter's shift every other update.** It would halve the
  jitter's cost, for a caller's rotation lagging by up to 63 frames; left
  until a profile asks.
