//! Measures how fast convolution runs, as a multiple of real time.
//!
//! ```text
//! cargo run --release -p manguebyte-dsp --example convolution_speed
//! ```
//!
//! Ten seconds of stereo noise at 48 kHz go through a
//! [`ConvolutionEngine`] with a three-second response per channel, in
//! blocks of 64 frames, with no latency (a direct head of one partition)
//! and with a latency of one partition, in the uniform scheme and the
//! non-uniform one, and through a [`FirFilter`] of 63 taps, which runs
//! directly. Each figure is the best of several runs. The direct head is
//! a dot product and the partitions are products of spectra, the two
//! loops `manguebyte_core::vector` speeds up. The uniform scheme's
//! 2 249 partitions of 64 samples against the non-uniform one's 24, of
//! sizes doubling up to 8 192, is where the difference comes from.

use std::hint::black_box;
use std::time::Instant;

use manguebyte_core::{AudioBuffer, Sample};
use manguebyte_dsp::{
    ConvolutionEngine, ConvolutionRouting, ConvolutionSettings, FirFilter, FirMode, ProcessSpec,
    Processor,
};

const RATE: f64 = 48_000.0;
const SECONDS: f64 = 10.0;
const RESPONSE_SECONDS: f64 = 3.0;
const BLOCK: usize = 64;
/// The largest partition of the non-uniform scheme: the bands double
/// from 128 to 8 192.
const LARGEST: usize = 8_192;

fn noise<S: Sample>(len: usize, seed: u64) -> Vec<S> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            S::from_f64((state >> 11) as f64 / (1u64 << 53) as f64 * 1.8 - 0.9)
        })
        .collect()
}

/// Times by real time: seconds of audio per second of work, the best of
/// several runs.
fn speed<S: Sample>(processor: &mut dyn Processor<S>) -> f64 {
    let frames = (RATE * SECONDS) as usize;
    let input: Vec<Vec<S>> = (0..2).map(|c| noise(BLOCK * 16, c + 7)).collect();
    let mut block = AudioBuffer::<S>::new(2, BLOCK);
    let mut best = f64::INFINITY;
    for _ in 0..3 {
        processor.reset();
        let start = Instant::now();
        let mut done = 0;
        while done < frames {
            let at = (done / BLOCK % 16) * BLOCK;
            for (channel, source) in input.iter().enumerate() {
                block
                    .channel_mut(channel)
                    .copy_from_slice(&source[at..at + BLOCK]);
            }
            processor.process(black_box(&mut block));
            done += BLOCK;
        }
        best = best.min(start.elapsed().as_secs_f64());
    }
    SECONDS / best
}

fn rows<S: Sample>(name: &str) {
    let response: Vec<Vec<S>> = (0..2)
        .map(|c| {
            noise::<S>((RESPONSE_SECONDS * RATE) as usize, c + 1)
                .into_iter()
                .enumerate()
                // A decaying tail, as a room's.
                .map(|(n, s)| s * S::from_f64((-(n as f64) / RATE * 3.0).exp()))
                .collect()
        })
        .collect();
    for zero_latency in [true, false] {
        for (scheme, largest) in [("uniform", BLOCK), ("non-uniform", LARGEST)] {
            let settings = ConvolutionSettings::new()
                .with_partition_size(BLOCK)
                .with_non_uniform_partitions(largest)
                .with_zero_latency(zero_latency);
            let mut engine =
                ConvolutionEngine::new(&response, 2, ConvolutionRouting::Parallel, settings)
                    .unwrap();
            println!(
                "| convolution, {RESPONSE_SECONDS} s stereo, {scheme}, {} | {name} | {:.1}x |",
                if zero_latency {
                    "no latency"
                } else {
                    "latency of one partition"
                },
                speed(&mut engine)
            );
        }
    }
    let taps: Vec<f64> = noise(63, 99);
    let mut fir = FirFilter::<S>::with_mode(&taps, FirMode::Direct).unwrap();
    fir.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: BLOCK,
        num_channels: 2,
    });
    println!(
        "| FIR, 63 taps, direct | {name} | {:.0}x |",
        speed(&mut fir)
    );
}

fn main() {
    println!("| processor | type | real time |");
    println!("|---|---|---:|");
    rows::<f32>("f32");
    rows::<f64>("f64");
}
