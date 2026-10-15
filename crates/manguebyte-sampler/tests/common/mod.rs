//! Shared helpers: generated samples, a driver that cuts the timeline into
//! blocks, and measurements.

#![allow(dead_code)]

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};
use manguebyte_formats::wav::{self, WavFormat};
use manguebyte_midi::MidiMessage;
use manguebyte_sampler::{Instrument, MemorySource, Sampler};

pub const RATE: f64 = 48_000.0;

/// A WAV file (32-bit float) holding `channels`, all at `rate`.
pub fn wav_bytes(channels: &[Vec<f32>], rate: u32) -> Vec<u8> {
    let frames = channels[0].len();
    let mut buffer = AudioBuffer::<f32>::new(channels.len(), frames);
    for (index, channel) in channels.iter().enumerate() {
        buffer.channel_mut(index).copy_from_slice(channel);
    }
    let mut bytes = Vec::new();
    wav::write(&mut bytes, &buffer, rate, WavFormat::Float32).unwrap();
    bytes
}

/// `frames` of a sine at `frequency`, with the given amplitude.
pub fn sine(frequency: f64, rate: f64, frames: usize, amplitude: f64) -> Vec<f32> {
    (0..frames)
        .map(|n| (amplitude * (core::f64::consts::TAU * frequency * n as f64 / rate).sin()) as f32)
        .collect()
}

/// A mono WAV of a sine.
pub fn sine_wav(frequency: f64, rate: u32, seconds: f64, amplitude: f64) -> Vec<u8> {
    let frames = (seconds * f64::from(rate)).round() as usize;
    wav_bytes(&[sine(frequency, f64::from(rate), frames, amplitude)], rate)
}

/// A mono WAV holding one value.
pub fn constant_wav(level: f32, frames: usize, rate: u32) -> Vec<u8> {
    wav_bytes(&[vec![level; frames]], rate)
}

/// Loads `text` against `source` and hands it to a prepared sampler at
/// 48 kHz with `channels` channels.
pub fn sampler_with(
    text: &str,
    source: &mut MemorySource,
    polyphony: usize,
    channels: usize,
) -> Sampler<f32> {
    let instrument = Instrument::from_sfz(text, source).unwrap_or_else(|e| panic!("{e}"));
    let (mut sampler, _loader) = Sampler::with_instrument(instrument, polyphony);
    sampler.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: 512,
        num_channels: channels,
    });
    sampler
}

pub fn note(key: u8) -> NoteRef {
    NoteRef::new(0, key)
}

pub fn on(key: u8, velocity: f32) -> EventKind {
    EventKind::NoteOn {
        note: note(key),
        velocity,
    }
}

pub fn off(key: u8) -> EventKind {
    EventKind::NoteOff {
        note: note(key),
        velocity: 0.0,
    }
}

pub fn controller(controller: u8, value: u8) -> EventKind {
    EventKind::Midi(MidiMessage::ControlChange {
        channel: 0,
        controller,
        value,
    })
}

pub fn bend(value: i16) -> EventKind {
    EventKind::Midi(MidiMessage::PitchBend { channel: 0, value })
}

/// Renders `frames` frames in blocks whose sizes cycle through `blocks`,
/// with each event on its absolute frame. Returns every channel.
pub fn render(
    sampler: &mut Sampler<f32>,
    channels: usize,
    frames: usize,
    events: &[(usize, EventKind)],
    blocks: &[usize],
) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::with_capacity(frames); channels];
    let largest = blocks.iter().copied().max().unwrap_or(1).max(1);
    let mut buffer = AudioBuffer::<f32>::new(channels, largest);
    let mut list = EventBuffer::with_capacity(events.len().max(1));
    let mut position = 0;
    let mut turn = 0;
    while position < frames {
        let size = blocks[turn % blocks.len()].max(1).min(frames - position);
        turn += 1;
        list.clear();
        for &(at, kind) in events {
            if at >= position && at < position + size {
                list.push(Event {
                    offset: (at - position) as u32,
                    kind,
                })
                .unwrap();
            }
        }
        buffer.set_num_frames(size);
        let transport = Transport::STOPPED;
        sampler.process_with(&mut buffer, &ProcessContext::new(&list, &transport));
        for (index, channel) in out.iter_mut().enumerate() {
            channel.extend_from_slice(buffer.channel(index));
        }
        position += size;
    }
    out
}

/// Renders one mono channel in blocks of 256.
pub fn render_mono(
    sampler: &mut Sampler<f32>,
    frames: usize,
    events: &[(usize, EventKind)],
) -> Vec<f32> {
    render(sampler, 1, frames, events, &[256]).remove(0)
}

/// Frequency from the rising zero crossings, located by linear
/// interpolation between frames.
pub fn frequency(signal: &[f32], rate: f64) -> f64 {
    let mut crossings = Vec::new();
    for n in 1..signal.len() {
        let (a, b) = (f64::from(signal[n - 1]), f64::from(signal[n]));
        if a < 0.0 && b >= 0.0 {
            crossings.push((n - 1) as f64 + a / (a - b));
        }
    }
    assert!(crossings.len() >= 3, "too few crossings to measure");
    let periods = (crossings.len() - 1) as f64;
    periods * rate / (crossings[crossings.len() - 1] - crossings[0])
}

/// Cents from `expected` to `measured`.
pub fn cents(measured: f64, expected: f64) -> f64 {
    1_200.0 * (measured / expected).log2()
}

/// The amplitude of the component at `frequency`, through a Hann window.
pub fn amplitude_at(signal: &[f32], frequency: f64, rate: f64) -> f64 {
    let len = signal.len();
    let (mut re, mut im, mut weights) = (0.0, 0.0, 0.0);
    for (n, &x) in signal.iter().enumerate() {
        let window = 0.5 - 0.5 * (core::f64::consts::TAU * n as f64 / len as f64).cos();
        let phase = core::f64::consts::TAU * frequency * n as f64 / rate;
        re += f64::from(x) * window * phase.cos();
        im += f64::from(x) * window * phase.sin();
        weights += window;
    }
    2.0 * (re * re + im * im).sqrt() / weights
}

/// Fits `a sin + b cos + c` at `frequency` by least squares and returns the
/// fitted amplitude and the level of what is left, relative to it, in dB.
pub fn sine_fit(signal: &[f32], frequency: f64, rate: f64) -> (f64, f64) {
    // Normal equations for the three basis functions.
    let mut m = [[0.0f64; 3]; 3];
    let mut v = [0.0f64; 3];
    let basis = |n: usize| {
        let phase = core::f64::consts::TAU * frequency * n as f64 / rate;
        [phase.sin(), phase.cos(), 1.0]
    };
    for (n, &x) in signal.iter().enumerate() {
        let b = basis(n);
        for i in 0..3 {
            v[i] += b[i] * f64::from(x);
            for j in 0..3 {
                m[i][j] += b[i] * b[j];
            }
        }
    }
    let c = solve3(m, v);
    let mut residual = 0.0;
    for (n, &x) in signal.iter().enumerate() {
        let b = basis(n);
        let fitted = c[0] * b[0] + c[1] * b[1] + c[2] * b[2];
        residual += (f64::from(x) - fitted).powi(2);
    }
    let amplitude = (c[0] * c[0] + c[1] * c[1]).sqrt();
    let residual_rms = (residual / signal.len() as f64).sqrt();
    let signal_rms = amplitude / 2f64.sqrt();
    (amplitude, 20.0 * (residual_rms / signal_rms).log10())
}

fn solve3(m: [[f64; 3]; 3], v: [f64; 3]) -> [f64; 3] {
    // Cramer's rule: small and exact enough for three unknowns.
    let det = |a: [[f64; 3]; 3]| {
        a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1])
            - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
            + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
    };
    let whole = det(m);
    let replaced = |column: usize| {
        let mut a = m;
        for (row, value) in a.iter_mut().zip(v) {
            row[column] = value;
        }
        det(a) / whole
    };
    [replaced(0), replaced(1), replaced(2)]
}

/// Root mean square.
pub fn rms(signal: &[f32]) -> f64 {
    (signal.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / signal.len().max(1) as f64).sqrt()
}

/// A small deterministic generator for tests.
pub struct Random(u64);

impl Random {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, limit: usize) -> usize {
        (self.next_u64() % limit.max(1) as u64) as usize
    }

    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}
