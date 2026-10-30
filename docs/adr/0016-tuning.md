# ADR 0016: Tuning: a table every voice copies, bends on top

Status: accepted, 2026-10-30

## Context

Until now every voice turned its key into hertz with one formula,
`440 · 2^((key − 69) / 12)`. Two products need more. Caecilia 2.0, a pipe
organ, plays historical temperaments at baroque and modern pitch with an
octave stretch, as its C++ 1.x did, and has reference tables from 1.x to
match. Parabolic, a synthesiser workstation, needs microtonal scales:
equal divisions other than 12, Scala files, and the MIDI Tuning Standard
that hardware and other software send tunings in. The framework should own
the tuning so both share one tested module, and JUCE has nothing to copy
here: it converts notes to hertz in equal temperament for a chosen A and
stops there.

Constraints:

- The audio thread asks for a frequency at every note, and many voices ask
  again every block or every sample for bends, glides and vibrato. A query
  must cost about what the formula costs, and must not lock or allocate.
- A tuning changes while notes play (a temperament switched on the
  console, a MIDI tuning dump). The change must reach the audio thread
  without allocating or freeing there (ADR 0002).
- Every existing voice and test must sound exactly as before when nothing
  asks for another tuning: block-size independence and renders compared
  bit for bit depend on it.
- Pitch bend has to mean something in a scale that is not 12 to the
  octave.

## Decision

**A tuning is a value with no heap memory.** `Tuning` holds two arrays of
128 numbers, each key's pitch and its frequency, and a bit per key for
whether it sounds. It is built off the audio thread, from settings, a
Scala file or MIDI tuning bytes, and never changed after. Being plain data,
it is moved through the wait-free queue of `manguebyte-core` by value and
dropped on the audio thread at no cost, so it needs neither the swap
pattern nor a queue to send old ones back (which instruments and tempo
maps use because they own heap memory).

**Pitch is kept as an equal-tempered note number**: the key whose 12-tone
equal temperament at A4 = 440 Hz would sound it, 69.0 for 440 Hz. It is the
unit of the MIDI Tuning Standard, and it makes the default tuning's
pitches the key numbers themselves, so the default reproduces the old
formula bit for bit for every note, whole or fractional, with the
interpolation written so that it is exact for it (`i + (i + 1 − i) · t` is
`i + t`, which is the note). A fractional note is interpolated linearly in
pitch between the keys around it and extrapolated beyond 0 and 127 with
the last step.

**Every voice keeps its own copy.** `Synth::set_tuning` copies the tuning
into the synth and into each voice through `Voice::set_tuning`, a default
method that ignores it. A copy is about 2 KB, a plain memory copy made at a
block boundary, never while a voice renders. Shared alternatives cost more where it
matters: an `Arc` would free the last reference on the audio thread
unless every swap is sent back for freeing, and passing a reference into
`render` would change the signature of every voice there is.

**Keys go through the tuning; bends go on top.** A voice takes its key's
pitch from the tuning (`Tuning::key_pitch`) and adds the pitch wheel,
per-note tuning, glides, vibrato and its own transpositions (oscillator
offsets, drawbar footages, an SFZ region's `tune` and note offset) as
intervals in semitones. This is what synthesizers that follow the MIDI
Tuning Standard do, it keeps a bend range of two semitones two semitones
in any scale, it keeps a polysynth's fifth above the key a fifth, and a
glide between two keys runs between their tuned pitches. A voice that
wants to bend in the tuning's own steps has `Tuning::frequency` of the
bent key.

**Unmapped keys are a property of the tuning**, from `x` entries of a
Scala keyboard mapping. `Synth` and `Sampler` do not start notes on them.
Their pitch is still defined, between the mapped keys around them, so
that interpolation and bends across them stay continuous.

Historical temperaments are built from their definitions, chains of
fifths narrowed by fractions of the Pythagorean or syntonic comma, not
copied as rounded cents; the tests compare them with the published values
and with Caecilia 1.x's tables.

## Consequences

- With no tuning set, nothing sounds different: every instrument and the
  sampler render bit for bit what they rendered before (checked by
  hashing the output of every instrument, drum kits included, and of the
  sampler, against the commit before tunings, and by tests that give a
  voice the default tuning explicitly).
- A tuning change reaches a sounding note only where the voice works its
  pitch out again; the instruments apply it from the next note at the
  latest. An organ that retunes held notes smoothly must do it itself.
- One tuning per synth. A tuning per MIDI channel, as multi-timbral MIDI
  Tuning Standard receivers keep, would be one table per channel in the
  synth and a voice told which one to use; not done.
- Sysex does not travel in the event stream, so MIDI tuning messages are
  read where the application receives them and the result handed over
  like any other tuning. Reading and applying one allocate nothing, so
  that place may be the audio thread.
- The cost of a query is a lookup for a key and a lerp and an `exp2` for
  a fractional note, the same order as the formula it replaces.

## Alternatives not taken

- **Bends through the tuning's keys** (a bend of one moves one key). In 19
  divisions of the octave the usual two-semitone wheel would cover 126
  cents and a polysynth's fifth would become seven nineteenths. Left to
  voices that want it, through `Tuning::frequency`.
- **A frequency table only.** Interpolating in hertz is not linear in
  pitch, and the default could not then match the formula for fractional
  notes.
- **A trait for tunings** evaluated on every query (an equal temperament
  computing its formula, a table looking up). It would make every query
  dynamic dispatch or make voices generic over it, for no gain over a
  table that every kind of tuning fills.
