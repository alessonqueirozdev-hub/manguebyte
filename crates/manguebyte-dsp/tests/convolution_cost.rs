//! Measures what a long convolution costs. Ignored by default, because the
//! numbers only mean something in an optimised build:
//!
//! ```text
//! cargo test -p manguebyte-dsp --release --test convolution_cost -- --ignored --nocapture
//! ```
//!
//! It reports the mean time per block, which sets the average load, and the
//! median time of the blocks at each place within a partition, the largest
//! of which is the block with the most work and decides whether a block can
//! miss its deadline. The slowest single block is also shown, but on a busy
//! machine it mostly measures other programs. Read all of them as rough
//! figures.

use std::time::Instant;

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::{ConvolutionEngine, ConvolutionRouting, ConvolutionSettings, Processor};

const RATE: usize = 48_000;
const BLOCK: usize = 64;

fn room(len: usize, seed: u32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(2_654_435_761) | 1;
    (0..len)
        .map(|n| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let unit = state as f32 / u32::MAX as f32 * 2.0 - 1.0;
            0.05 * unit * (-6.0 * n as f32 / len as f32).exp()
        })
        .collect()
}

#[test]
#[ignore = "a measurement, not a check; run it in release mode"]
fn a_two_second_stereo_response_in_blocks_of_64() {
    let len = 2 * RATE;
    let seconds = 10;
    let blocks = seconds * RATE / BLOCK;
    let deadline_us = 1e6 * BLOCK as f64 / RATE as f64;
    println!("one block of {BLOCK} frames lasts {deadline_us:.0} us");
    for (routing, responses) in [
        (ConvolutionRouting::Parallel, 2),
        (ConvolutionRouting::TrueStereo, 4),
    ] {
        let responses: Vec<Vec<f32>> = (0..responses).map(|seed| room(len, seed)).collect();
        for (partition, zero_latency) in [
            (64, true),
            (128, true),
            (256, true),
            (64, false),
            (128, false),
            (256, false),
        ] {
            let settings = ConvolutionSettings::new()
                .with_partition_size(partition)
                .with_zero_latency(zero_latency);
            let mut engine = ConvolutionEngine::new(&responses, 2, routing, settings).unwrap();
            let mut buffer = AudioBuffer::<f32>::new(2, BLOCK);
            // Blocks fall at the same place in each partition every
            // `phases` blocks. The median time at each place is robust to
            // other programs taking the processor now and then; the
            // largest of those medians is the block that does the most work.
            let phases = (partition / BLOCK).max(1);
            let mut times = vec![Vec::with_capacity(blocks / phases + 1); phases];
            let start = Instant::now();
            for block in 0..blocks {
                for channel in buffer.iter_channels_mut() {
                    channel.fill(if block % 100 == 0 { 0.5 } else { 0.0 });
                }
                let before = Instant::now();
                engine.process(&mut buffer);
                times[block % phases].push(before.elapsed().as_secs_f64() * 1e6);
            }
            let elapsed = start.elapsed().as_secs_f64();
            let mut slowest = 0.0f64;
            let medians: Vec<String> = times
                .iter_mut()
                .map(|phase| {
                    phase.sort_by(f64::total_cmp);
                    slowest = slowest.max(phase[phase.len() - 1]);
                    format!("{:.0}", phase[phase.len() / 2])
                })
                .collect();
            println!(
                "{routing:?}, partition {partition}, latency {}: {:.1}% of one core, \
                 mean {:.1} us per block, median by place in the partition [{}] us, \
                 slowest block seen {:.0} us",
                engine.latency(),
                100.0 * elapsed / seconds as f64,
                1e6 * elapsed / blocks as f64,
                medians.join(", "),
                slowest,
            );
            assert!(
                buffer
                    .iter_channels()
                    .all(|c| c.iter().all(|s| s.is_finite()))
            );
        }
    }
}
