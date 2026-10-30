# manguebyte-dsp

Signal processing for the Manguebyte Audio Framework: oscillators, wavetables, filters, EQ, dynamics with sidechain, delay, modulation, reverb, convolution, FIR, spectral processing by STFT and overlap-add, oversampling, a polyphonic synth with instrument voices (plucked strings, FM, tonewheel organ, acid bass, mallets by modal synthesis, bowed strings and winds by digital waveguide, choirs by formant synthesis, drum kits: acoustic by modal synthesis, Brazilian percussion by physical modelling, and drum machines, a deep polyphonic synthesiser with 68 presets) and zero-cost processor chains.

Part of the [Manguebyte Audio Framework](https://github.com/alessonqueirozdev-hub/manguebyte). Most projects depend on
[`manguebyte`](https://crates.io/crates/manguebyte), which gathers every part
under one name; depend on this crate directly to use only this part.

Documentation: <https://docs.rs/manguebyte-dsp>

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

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
