//! Pitch and level set from outside on every block, as Caecilia's wind
//! model moves every pipe: `Sampler::modulate_key`, `modulate_all` and
//! `SamplerVoice::modulate`, ramped to the frame, and the public
//! `SincReader` with a ratio for every frame.

mod common;

use std::sync::Arc;

use common::*;
use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::NoteRef;
use manguebyte_dsp::{ProcessSpec, Processor, Synth};
use manguebyte_sampler::{Instrument, MemorySource, Sampler, SamplerVoice, SincReader};

const CARRIER: f64 = 1_000.0;
const RATE_HZ: f64 = 5.0;
const DEPTH_CENTS: f64 = 2.0;
const BLOCK: usize = 64;
const FRAMES: usize = 96_000;

fn source() -> MemorySource {
    MemorySource::new().with("tone.wav", sine_wav(CARRIER, 48_000, 3.0, 0.5))
}

/// The ratio of the wind's frequency modulation at frame `n`.
fn ratio_at(n: usize) -> f64 {
    let t = n as f64 / RATE;
    2f64.powf(DEPTH_CENTS * (core::f64::consts::TAU * RATE_HZ * t).sin() / 1_200.0)
}

/// Frequencies, each period's from its rising zero crossings, with the
/// time of each period's middle in frames.
fn periods(signal: &[f32]) -> Vec<(f64, f64)> {
    let mut crossings = Vec::new();
    for n in 1..signal.len() {
        let (a, b) = (f64::from(signal[n - 1]), f64::from(signal[n]));
        if a < 0.0 && b >= 0.0 {
            crossings.push(n as f64 - 1.0 + a / (a - b));
        }
    }
    crossings
        .windows(2)
        .map(|pair| (0.5 * (pair[0] + pair[1]), RATE / (pair[1] - pair[0])))
        .collect()
}

/// The depth, in cents, of a sinusoidal modulation at [`RATE_HZ`] of the
/// frequencies `periods` gives, by least squares.
fn depth_cents(periods: &[(f64, f64)]) -> f64 {
    let (mut ss, mut sc, mut cc, mut ys, mut yc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for &(at, frequency) in periods {
        let angle = core::f64::consts::TAU * RATE_HZ * at / RATE;
        let cents = 1_200.0 * (frequency / CARRIER).log2();
        let (s, c) = angle.sin_cos();
        ss += s * s;
        sc += s * c;
        cc += c * c;
        ys += cents * s;
        yc += cents * c;
    }
    let det = ss * cc - sc * sc;
    let a = (ys * cc - yc * sc) / det;
    let b = (yc * ss - ys * sc) / det;
    a.hypot(b)
}

/// What a modulated sine should read: the ratios of a block's ramp, from
/// the last block's value to this one's on its last frame, added up.
fn expected(frames: usize, ratio: impl Fn(usize) -> f64) -> Vec<f32> {
    let mut output = Vec::with_capacity(frames);
    let mut position = 0.0f64;
    let mut current = 1.0;
    for block in 0..frames.div_ceil(BLOCK) {
        let end = ((block + 1) * BLOCK).min(frames);
        let target = ratio(end - 1);
        let length = end - block * BLOCK;
        for i in 0..length {
            output.push((0.5 * (core::f64::consts::TAU * CARRIER * position / RATE).sin()) as f32);
            position += current + (target - current) * (i + 1) as f64 / length as f64;
        }
        current = target;
    }
    output
}

#[test]
fn two_cents_of_fm_through_the_sampler() {
    let mut source = source();
    let instrument =
        Instrument::from_sfz("<region> sample=tone.wav pitch_keycenter=69", &mut source).unwrap();
    let (mut sampler, _loader) = Sampler::<f32>::with_instrument(instrument, 4);
    sampler.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: BLOCK,
        num_channels: 1,
    });
    sampler.note_on(NoteRef::new(0, 69), 1.0);
    let mut buffer = AudioBuffer::<f32>::new(1, BLOCK);
    let mut output = Vec::with_capacity(FRAMES);
    for block in 0..FRAMES / BLOCK {
        // The model's value for the block's last frame.
        let ratio = ratio_at((block + 1) * BLOCK - 1);
        sampler.modulate_key(69, ratio as f32, 1.0);
        sampler.process(&mut buffer);
        output.extend_from_slice(buffer.channel(0));
    }
    let wanted = expected(FRAMES, |n| f64::from(ratio_at(n) as f32));
    let error = output
        .iter()
        .zip(&wanted)
        .skip(64)
        .map(|(a, b)| f64::from((a - b).abs()))
        .fold(0.0, f64::max);
    let depth = depth_cents(&periods(&output[4_800..]));
    println!("2 cents at 5 Hz: measured {depth:.4} cents, largest error {error:.2e}");
    assert!((depth - DEPTH_CENTS).abs() < 0.01, "{depth}");
    assert!(error < 1e-4, "{error}");
}

#[test]
fn a_gain_ramps_over_one_block_and_other_keys_are_left_alone() {
    let mut source = source();
    let text = "<region> sample=tone.wav key=69 pitch_keycenter=69
<region> sample=tone.wav key=70 pitch_keycenter=70 volume=-200";
    let instrument = Instrument::from_sfz(text, &mut source).unwrap();
    let (mut sampler, _loader) = Sampler::<f32>::with_instrument(instrument, 4);
    sampler.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: 480,
        num_channels: 1,
    });
    sampler.note_on(NoteRef::new(0, 69), 1.0);
    let mut buffer = AudioBuffer::<f32>::new(1, 480);
    sampler.process(&mut buffer);
    let plain: Vec<f32> = buffer.channel(0).to_vec();
    // Key 70's modulation does not touch key 69.
    sampler.modulate_key(70, 2.0, 0.0);
    sampler.process(&mut buffer);
    let tone = sine(CARRIER, RATE, 960, 0.5);
    assert!(
        buffer
            .channel(0)
            .iter()
            .zip(&tone[480..])
            .all(|(a, b)| a == b)
    );
    assert!(plain.iter().zip(&tone).all(|(a, b)| a == b));
    // Half the level, along a line over the next block, then held.
    sampler.modulate_all(1.0, 0.5);
    sampler.process(&mut buffer);
    let tone = sine(CARRIER, RATE, 1_920, 0.5);
    for (i, (&a, &b)) in buffer.channel(0).iter().zip(&tone[960..]).enumerate() {
        let gain = 1.0 - 0.5 * (i + 1) as f32 / 480.0;
        assert!((a - b * gain).abs() < 1e-6, "{i}");
    }
    sampler.process(&mut buffer);
    for (&a, &b) in buffer.channel(0).iter().zip(&tone[1_440..]) {
        assert!((a - 0.5 * b).abs() < 1e-6);
    }
}

#[test]
fn a_sampler_voice_takes_the_same_modulation() {
    let mut source = source();
    let instrument = Arc::new(
        Instrument::from_sfz("<region> sample=tone.wav pitch_keycenter=69", &mut source).unwrap(),
    );
    let mut synth = Synth::with_voices(1, || SamplerVoice::new(Arc::clone(&instrument)));
    synth.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: BLOCK,
        num_channels: 1,
    });
    synth.note_on(NoteRef::new(0, 69), 1.0);
    let mut buffer = AudioBuffer::<f32>::new(1, BLOCK);
    let mut output = Vec::with_capacity(FRAMES);
    for block in 0..FRAMES / BLOCK {
        let ratio = ratio_at((block + 1) * BLOCK - 1);
        for voice in synth.voices_mut() {
            voice.modulate(ratio as f32, 1.0, BLOCK);
        }
        synth.process(&mut buffer);
        output.extend_from_slice(buffer.channel(0));
    }
    let depth = depth_cents(&periods(&output[4_800..]));
    assert!((depth - DEPTH_CENTS).abs() < 0.01, "{depth}");
}

#[test]
fn the_sinc_reader_follows_a_ratio_for_every_frame() {
    let input = sine(CARRIER, RATE, FRAMES + 2_000, 0.5);
    let ratios: Vec<f32> = (0..FRAMES).map(|n| ratio_at(n) as f32).collect();
    let mut reader = SincReader::new();
    let mut output = vec![0.0f32; FRAMES];
    // In pieces of any size, the same as in one.
    let mut at = 0;
    for size in [1, 63, 1_000, 4_096].iter().cycle() {
        if at >= FRAMES {
            break;
        }
        let end = (at + size).min(FRAMES);
        reader.process(&input, &ratios[at..end], &mut output[at..end]);
        at = end;
    }
    let mut whole = vec![0.0f32; FRAMES];
    SincReader::new().process(&input, &ratios, &mut whole);
    assert_eq!(output, whole);
    let mut position = 0.0f64;
    let mut error: f64 = 0.0;
    for (n, &value) in output.iter().enumerate() {
        let wanted = 0.5 * (core::f64::consts::TAU * CARRIER * position / RATE).sin();
        if n > 64 {
            error = error.max((f64::from(value) - wanted).abs());
        }
        position += f64::from(ratios[n]);
    }
    let depth = depth_cents(&periods(&output[4_800..]));
    println!("sinc reader: measured {depth:.4} cents, largest error {error:.2e}");
    assert!((depth - DEPTH_CENTS).abs() < 0.01, "{depth}");
    assert!(error < 1e-4, "{error}");
    assert!((reader.position() - position).abs() < 1e-3);
}
