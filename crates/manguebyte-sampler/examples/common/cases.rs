//! The cases the benchmarks time, shared by the native benchmark and the
//! WebAssembly one.

#![allow(dead_code)]

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::NoteRef;
use manguebyte_dsp::{ProcessSpec, Processor};
use manguebyte_formats::wav::{self, WavFormat};
use manguebyte_sampler::{Instrument, Interpolation, MemorySource, Sampler};

/// Output rate of every case.
pub const RATE: f64 = 48_000.0;

/// Frames per block.
pub const BLOCK: usize = 128;

/// Voices sounding in every case.
pub const VOICES: usize = 32;

/// One way of playing the sample.
pub struct Case {
    /// What the case is, for the table.
    pub name: &'static str,
    /// How the voices read the sample.
    pub interpolation: Interpolation,
    /// Rate of the sample file.
    pub sample_rate: u32,
    /// Key played; the sample sounds unchanged at 60.
    pub key: u8,
}

/// Every case, in the order the table shows them.
pub const CASES: &[Case] = &[
    Case {
        name: "sinc, own pitch and rate",
        interpolation: Interpolation::Sinc,
        sample_rate: 48_000,
        key: 60,
    },
    Case {
        name: "sinc, 44.1 kHz sample at its pitch",
        interpolation: Interpolation::Sinc,
        sample_rate: 44_100,
        key: 60,
    },
    Case {
        name: "sinc, down a fifth",
        interpolation: Interpolation::Sinc,
        sample_rate: 48_000,
        key: 53,
    },
    Case {
        name: "sinc, up a fifth",
        interpolation: Interpolation::Sinc,
        sample_rate: 48_000,
        key: 67,
    },
    Case {
        name: "sinc, up an octave",
        interpolation: Interpolation::Sinc,
        sample_rate: 48_000,
        key: 72,
    },
    Case {
        name: "sinc, up two octaves",
        interpolation: Interpolation::Sinc,
        sample_rate: 48_000,
        key: 84,
    },
    Case {
        name: "cubic, down a fifth",
        interpolation: Interpolation::Cubic,
        sample_rate: 48_000,
        key: 53,
    },
    Case {
        name: "cubic, up a fifth",
        interpolation: Interpolation::Cubic,
        sample_rate: 48_000,
        key: 67,
    },
];

/// A stereo sampler at 48 kHz playing [`VOICES`] notes of `case`, all
/// looping, ready to render.
pub fn sampler(case: &Case) -> Sampler<f32> {
    let frames = case.sample_rate as usize;
    let mut tone = AudioBuffer::<f32>::new(2, frames);
    for channel in 0..2 {
        for (n, sample) in tone.channel_mut(channel).iter_mut().enumerate() {
            let phase = core::f64::consts::TAU * 440.0 * n as f64 / f64::from(case.sample_rate);
            *sample = (0.1 * (phase + channel as f64).sin()) as f32;
        }
    }
    let mut bytes = Vec::new();
    wav::write(&mut bytes, &tone, case.sample_rate, WavFormat::Float32).expect("write");
    let mut source = MemorySource::new().with("tone.wav", bytes);
    let instrument = Instrument::from_sfz(
        "<region> sample=tone.wav loop_mode=loop_continuous ampeg_release=1",
        &mut source,
    )
    .expect("instrument");
    let (mut sampler, _loader) = Sampler::<f32>::with_instrument(instrument, VOICES);
    sampler.set_interpolation(case.interpolation);
    sampler.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: BLOCK,
        num_channels: 2,
    });
    for id in 0..VOICES as u32 {
        sampler.note_on(NoteRef::with_id(0, case.key, id), 0.8);
    }
    sampler
}

/// Renders `blocks` blocks and returns a sum of the output, so that the
/// work cannot be optimised away.
pub fn render(sampler: &mut Sampler<f32>, buffer: &mut AudioBuffer<f32>, blocks: usize) -> f64 {
    let mut sum = 0.0;
    for _ in 0..blocks {
        sampler.process(buffer);
        sum += f64::from(buffer.channel(0)[0]) + f64::from(buffer.channel(1)[BLOCK - 1]);
    }
    sum
}
