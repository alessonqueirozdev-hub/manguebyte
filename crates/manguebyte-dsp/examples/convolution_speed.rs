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
//! directly. Every run is timed as five two-second segments, the rows of
//! the table taking turns segment by segment over three rounds, and each
//! figure is the best of the fifteen, so a burst of background load hits
//! every row rather than one row's whole attempt. The direct head is a
//! dot product and the partitions are products of spectra, the two loops
//! `manguebyte_core::vector` speeds up. The uniform scheme's
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
const SECONDS: f64 = 2.0;
const RESPONSE_SECONDS: f64 = 3.0;
const BLOCK: usize = 64;
/// The largest partition of the non-uniform scheme: the bands double
/// from 128 to 8 192.
const LARGEST: usize = 8_192;
/// The rounds through the table, and the timed segments of a row in
/// each: `RUNS * SEGMENTS` measurements a row, the best kept.
const RUNS: usize = 3;
const SEGMENTS: usize = 5;

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

/// One row of the table: the label, the sample type, the processor and
/// its best time so far.
struct Row<S: Sample> {
    label: String,
    name: &'static str,
    processor: Box<dyn Processor<S>>,
    best: f64,
}

/// Times by real time: seconds of audio per second of work, one run.
fn speed<S: Sample>(processor: &mut dyn Processor<S>) -> f64 {
    let frames = (RATE * SECONDS) as usize;
    let input: Vec<Vec<S>> = (0..2).map(|c| noise(BLOCK * 16, c + 7)).collect();
    let mut block = AudioBuffer::<S>::new(2, BLOCK);
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
    SECONDS / start.elapsed().as_secs_f64()
}

fn rows<S: Sample>(name: &'static str) -> Vec<Row<S>> {
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
    let mut rows: Vec<Row<S>> = Vec::new();
    for zero_latency in [true, false] {
        for (scheme, largest) in [("uniform", BLOCK), ("non-uniform", LARGEST)] {
            let mut settings = ConvolutionSettings::new()
                .with_partition_size(BLOCK)
                .with_zero_latency(zero_latency);
            if largest != BLOCK {
                settings = settings.with_non_uniform_partitions(largest);
            }
            let engine =
                ConvolutionEngine::new(&response, 2, ConvolutionRouting::Parallel, settings)
                    .unwrap();
            rows.push(Row {
                label: format!(
                    "convolution, {RESPONSE_SECONDS} s stereo, {scheme}, {}",
                    if zero_latency {
                        "no latency"
                    } else {
                        "latency of one partition"
                    }
                ),
                name,
                processor: Box::new(engine),
                best: 0.0,
            });
        }
    }
    let taps: Vec<f64> = noise(63, 99);
    let mut fir = FirFilter::<S>::with_mode(&taps, FirMode::Direct).unwrap();
    fir.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: BLOCK,
        num_channels: 2,
    });
    rows.push(Row {
        label: "FIR, 63 taps, direct".to_string(),
        name,
        processor: Box::new(fir),
        best: 0.0,
    });
    rows
}

/// Times every row `RUNS * SEGMENTS` times, the rows taking turns
/// segment by segment, and prints the best of each: background load that
/// lasts a few seconds hits every row's segments rather than one row's
/// whole set of attempts.
fn print_table<S: Sample>(mut rows: Vec<Row<S>>) {
    for _ in 0..RUNS {
        for _ in 0..SEGMENTS {
            for row in &mut rows {
                row.best = row.best.max(speed(&mut *row.processor));
            }
        }
    }
    for row in &rows {
        println!("| {} | {} | {:.1}x |", row.label, row.name, row.best);
    }
}

fn main() {
    println!("| processor | type | real time |");
    println!("|---|---|---:|");
    print_table(rows::<f32>("f32"));
    print_table(rows::<f64>("f64"));
}
