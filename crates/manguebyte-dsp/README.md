# manguebyte-dsp

Signal processing for the Manguebyte Audio Framework: oscillators, wavetables, filters, EQ, dynamics with sidechain, delay, modulation, reverb, convolution, FIR, spectral processing by STFT and overlap-add, oversampling, a polyphonic synth with instrument voices (plucked strings, FM, tonewheel organ, acid bass, mallets by modal synthesis, bowed strings and winds by digital waveguide, choirs by formant synthesis, drum kits: acoustic by modal synthesis, Brazilian percussion by physical modelling, and drum machines, a deep polyphonic synthesiser with 68 presets), tunings beyond equal temperament (historical temperaments, Scala files, the MIDI Tuning Standard) and zero-cost processor chains.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

Documentation: <https://docs.rs/manguebyte-dsp>

## Effects

Besides the filters, dynamics, delay, chorus, flanger, phaser, reverb,
convolution and spectral effects, the crate has the effects a synth
workstation needs, each a `Processor` with real-time-safe setters that
glide, tested with numbers:

- `Bitcrusher`: bit depth as a continuous value, optional triangular
  dither, rate reduction by holding, with an optional elliptic
  anti-aliasing low-pass.
- `Tremolo` and `AutoPan`: level or position moved by an `Lfo`, in hertz
  or locked to the transport's tempo, with a stereo phase offset.
- `RingModulator`: by its own cosine carrier or by the sidechain.
- `FrequencyShifter`: every partial moved by the same hertz, up or down,
  through an elliptic all-pass 90-degree network; both sidebands from
  `process_sample`.
- `StereoWidener`: mid/side width that never touches the mono sum, with
  optional bass mono below a cutoff.
- `TransientShaper`: attack and sustain gains from envelope followers.
- `MultibandCompressor`: three Linkwitz-Riley bands, a `Compressor` on
  each; idle bands sum flat.
- `TapeSaturation`: emphasis, an oversampled asymmetric curve, and wow and
  flutter with seeded drift.
- `Vocoder`: 8 to 32 bands, the modulator from the sidechain, with formant
  shift.
- `GranularDelay`: windowed grains with position spray, pitch per grain,
  stereo spread and feedback, from a fixed pool of 64.
- `FormantFilter`: vowels A-E-I-O-U from parallel resonances, morphed.

## Transport and tempo map

A host tells a processor where its transport is (`Transport`, in the
`ProcessContext` of every block). For a program that keeps time itself, the
`timeline` module has the other side:

- `TempoMap`: tempo points (steps and linear ramps) and time signatures on
  a musical timeline, with closed-form conversions between frames, seconds,
  quarter notes (PPQ) and bars, beats and ticks, in both directions. A ramp
  is linear in time, as CLAP's `tempo_inc` describes one; the map's
  documentation gives the arithmetic. `TempoMap::from_midi_file` reads a
  Standard MIDI File's tempo map.
- `TransportClock`: plays a map block by block, with play, stop, seeking
  and a loop. Each block gives its `Transport`, the pieces it is cut into
  where the loop goes round, and the frame on which any beat, tempo point
  or bar line lands, so that events go on exact frames. Positions do not
  depend on the block sizes, and advancing never allocates.
- `ClockControl`: works a clock from another thread through the wait-free
  queues of `manguebyte-core`, including handing it a new map.

```rust
use manguebyte_dsp::timeline::{TempoCurve, TempoMap, TimeSignature, TransportClock};

let mut map = TempoMap::new(120.0, TimeSignature::FOUR_FOUR).unwrap();
// A ramp from bar 2 to 140 bpm at bar 4.
map.set_tempo(8.0, 120.0, TempoCurve::Linear).unwrap();
map.set_tempo(16.0, 140.0, TempoCurve::Step).unwrap();
let (mut clock, control) = TransportClock::with_control(map, 48_000.0);
control.set_loop_beats(Some((8.0, 16.0)));
control.play();

// On the audio thread:
let block = clock.next_block(512);
let transport = block.transport(clock.map());
```

The plugin `Harness` and the standalone runner drive their plugins with a
`TransportClock`, and hand the plugin the map in `ProcessContext::timeline`.

## Spectral processing

The `stft` module changes sound in the frequency domain. `Stft` takes a
frame every hop, windows it, runs the real FFT, hands the spectrum to a
`SpectralProcessor` (or a closure), transforms back, windows again and
overlap-adds:

- Any transform size up to 65 536 and any hop, with any window or its
  square root on either side, as long as the two windows overlap-add to a
  constant at that hop. `Stft::new` checks the sum and refuses the rest
  with `StftError::NotOverlapAdd`: Hann on both sides needs a hop of a
  third of the frame or less, Hann with a rectangular synthesis window
  half or less.
- An unchanged spectrum gives back the input exactly, delayed by the
  reported latency: `fft_size - 1`, less the synthesis window's leading
  zeros (`fft_size - 2` for Hann).
- Frames fall at fixed points of the stream, so the output is the same bit
  for bit however the host cuts its blocks. Mono or any number of
  channels; nothing is allocated after `prepare`.

Built on it: `SpectralGate` (a threshold per bin, and a learned noise
profile that makes it a simple denoiser), `SpectralFreeze` and a basic
phase-vocoder `PitchShift`.

```rust
use manguebyte_dsp::fft::Complex;
use manguebyte_dsp::stft::{SpectralFrame, SpectralGate, Stft, StftSettings};

// A brick-wall low-pass, as a closure on each spectrum.
let lowpass = Stft::from_fn(StftSettings::new(1_024), |frame: SpectralFrame<'_, f32>| {
    frame.bins[64..].fill(Complex::ZERO);
})
.unwrap();
assert_eq!(lowpass.latency(), 1_022);

// A denoiser: learn the noise, then keep what rises 10 dB above it.
let mut denoiser = Stft::<_, f32>::new(StftSettings::new(2_048), SpectralGate::new(-90.0)).unwrap();
denoiser.processor_mut().set_learning(true);
// ... process a stretch of noise, then:
denoiser.processor_mut().set_learning(false);
```

## Additive synthesis

`PartialBank` holds a bank of sine partials, each a quadrature phasor
turned by a fixed rotation every frame, under a gain that can ramp, with
its own pan, and adds them into a stereo pair of blocks. It is built for
an additive voice whose settings change every control block, such as
Caecilia's organ pipes:

- `set_rotation` takes the `(cos w, sin w)` a caller already has and costs
  two stores; `set_frequency` works them out from hertz. `ramp_to` moves
  a gain linearly over a number of frames, a step added before each
  sample, as Caecilia's kernel does.
- It renders on `manguebyte_core::vector::add_partials`: one partial per
  lane of a vector register, 16 at a time. Every backend gives the same
  output bit for bit, and each partial's numbers are those of Caecilia's
  scalar kernel bit for bit; only the order in which a frame's partials
  are added differs, within a stated bound.
- Every 64 frames each phasor is brought back to the unit circle with one
  Newton step, so its amplitude stays within 3e-5 of where it should be
  however long it runs. Nothing allocates after `new`, and how the caller
  cuts its blocks does not change the output.

Caecilia's tutti, 3 000 partials with new rotations and ramps every 64
frames (`tests/partial_bank_cost.rs`, on an Intel Core i3-1215U under
load): Caecilia's scalar kernel takes a whole core; the bank on AVX2 takes
0.16 of one as a single bank (6.4 to 7.7 times faster) and 0.23 as 200
banks of 15 (4.2 times faster), SSE2 3.2 to 4.2 times faster.

```rust
use manguebyte_dsp::{Partial, PartialBank};

let mut bank = PartialBank::new(64);
bank.set_sample_rate(48_000.0);
bank.set_len(3);
for (i, ratio) in [1.0, 2.0, 3.0].into_iter().enumerate() {
    bank.set(i, Partial { freq_hz: 220.0 * ratio, gain: 0.0, pan: 0.0 });
    bank.ramp_to(i, 0.3 / ratio as f32, 64);
}
let (mut left, mut right) = (vec![0.0f32; 256], vec![0.0f32; 256]);
bank.render_add(&mut left, &mut right);
```

By themselves the partials are perfectly clean and steady. The bank can make
them sound blown instead, three ways, all multiplying what the caller sets, so
`gain`, `ramp_to` and `set_rotation` mean what they did (ADR 0026):

- `set_air` puts narrow-band noise around a partial, of a depth and a
  bandwidth, by multiplying its phasor by `1 + depth · n(t)` on the way to the
  output: a skirt shaped like a resonance's, with the partial's own level
  where it was.
- `start_attack` makes a partial speak: a delay, a rise, an overshoot that
  settles, a breath of extra air that starts wide and narrows, a pitch that
  settles. Upper partials can speak first. `restart_attack` does so from the
  level a sounding partial has now, for a key struck again during a release.
- `set_wind_jitter` wanders the whole bank's gain and frequency, by the same
  ratio for every partial, on top of what a bellows model sets.

`PipeVoicing::FLUE_PRINCIPAL` works these out for a flue principal, as a
starting point: most of its numbers are judgements, and say so.
`set_voicing_seed` picks the noise: give every pipe its own. Everything
updates every 32 frames, with straight lines between. A bank with none of it
on runs the plain kernel and gives the same bits as if it did not exist; with
it, the output is the same whatever the blocks and on the backends run (scalar,
SSE2, AVX and AVX2 on one x86-64 machine; NEON and WebAssembly SIMD have not
been run). 2 000 partials at 48 kHz cost 1.4 to 1.6 times the plain kernel with
air (0.05 to 0.11 of a core on AVX2), 1.6 to 2.1 with air and jitter (0.07 to
0.13), and 1.8 to 2.5 with all three and every partial always in an attack
(0.08 to 0.15; `examples/partial_voicing_speed.rs`, one logical processor of an
Intel Core i3-1215U). `examples/organ_pipe_wavs.rs` writes a pipe and a chord,
plain and voiced, to compare by ear.

```rust
use manguebyte_dsp::{Partial, PartialBank, PipeVoicing};

let voicing = PipeVoicing::FLUE_PRINCIPAL;
let mut bank = PartialBank::new(16);
bank.set_sample_rate(48_000.0);
bank.set_len(8);
bank.set_voicing_seed(60);
bank.set_wind_jitter(voicing.jitter);
for i in 0..8 {
    let harmonic = (i + 1) as f32;
    let hz = 261.63 * f64::from(harmonic);
    bank.set(i, Partial { freq_hz: hz, gain: 0.2 / harmonic, pan: 0.0 });
    bank.set_air(i, voicing.air(harmonic, hz));
    bank.start_attack(i, &voicing.attack(harmonic, 48_000.0));
}
let (mut left, mut right) = (vec![0.0f32; 4_800], vec![0.0f32; 4_800]);
bank.render_add(&mut left, &mut right);
```

## The space around the pipes

The `space` module puts an organ's pipes in a room, for an organ that
renders each pipe as its own mono signal:

- `Listener::place` turns a pipe's position in metres into a `Placement`:
  a pan between loudspeakers (`Speakers`, stereo or up to 16 channels,
  equal power), a delay of the distance over 343 m/s, a gain of one over
  the distance, and a gentle low-pass for the air whose cutoff falls with
  distance. `Chest` lays a rank out across a windchest: chromatic, C and
  C♯ sides, or a mitre.
- `Windchest` places thousands of pipes and costs only for those that
  sound: each sounding pipe's block is filtered, delayed by a fraction of
  a sample through four Lagrange taps, and written ahead into one
  accumulation ring per output channel, in fixed point, so the output is
  the same bit for bit whatever the block size and whatever order the
  pipes come in. Moving a sounding pipe crossfades over 20 ms.
- `Reflections` and `EarlyReflections`: the early reflections of a
  rectangular nave by image sources, up to order 6, with each wall's
  absorption and an optional one-pole, played from a division's bus;
  `keep_tail` trims a measured response to follow them as the
  `Convolution` tail.
- `SwellBox`: an enclosed division's shutters, one opening from 0 to 1
  setting a level and a cutoff, smoothed so the pedal never steps.

300 sounding pipes into stereo, 128 frames at 48 kHz, take 0.22 to
0.28 ms a block on a busy Intel Core i3-1215U (`examples/space_speed.rs`),
early reflections of order 3 about 0.04 ms.

```rust
use manguebyte_core::AudioBuffer;
use manguebyte_dsp::space::{Listener, Position, Speakers, Windchest, WindchestSettings};

let listener = Listener::new(Position::new(0.0, 0.0, 1.6), Speakers::stereo());
let mut chest = Windchest::new(&WindchestSettings::new(2_000, 2, 48_000.0, 128)).unwrap();
chest.set_placement(0, &listener.place(Position::new(-2.0, 15.0, 4.0)));

// Each block, for each sounding pipe, then once:
chest.add(0, &[0.1; 128]);
let mut out = AudioBuffer::<f32>::new(2, 128);
chest.finish(&mut out);
```

## Tuning

The `tuning` module gives every key of a `Synth` or a sampler a pitch other
than 12-tone equal temperament's. A `Tuning` is a table of the 128 keys'
pitches and frequencies, built off the audio thread from `TuningSettings`:

- equal divisions of the octave or of any period (19, 24, 31, 53, or 13 to
  the tritave);
- twelve-note `Temperament`s on any key, built from their definitions:
  Pythagorean, quarter-comma meantone, Werckmeister III, Kirnberger III,
  Vallotti, Young's second and five-limit just intonation, or any table of
  cents;
- Scala scales (`.scl`) and keyboard mappings (`.kbm`), with unmapped
  keys, any period and the line of every error;
- any reference key and frequency, a transposition, a detune and an
  octave `Stretch`; and tunings sent as MIDI Tuning Standard messages
  (`MtsMessage`: bulk dumps and single note changes).

It holds no heap memory, so it goes to the audio thread through a queue and
`Synth::set_tuning` copies it into every voice without allocating. The
instruments take their key's pitch from it and add bends and glides on
top in semitones. `Tuning::default()` is 12-tone equal temperament at
A4 = 440 Hz and gives the frequencies the voices always had, bit for bit.
The temperaments are checked against their published cents and against
Caecilia 1.x's reference tables.

```rust
use manguebyte_dsp::tuning::{Scale, Temperament, TuningSettings};

let baroque = TuningSettings {
    scale: Scale::Temperament { temperament: Temperament::werckmeister_iii(), key: 0 },
    reference_hz: 415.0,
    ..TuningSettings::default()
}
.build()
.unwrap();
assert!((baroque.key_frequency(69) - 415.0).abs() < 1e-9);
// synth.set_tuning(&baroque);
```

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
