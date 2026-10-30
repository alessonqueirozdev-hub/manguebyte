//! Times `manguebyte_core::pool::RenderPool` on a CPU-heavy synthetic
//! instrument: 64 voices of additive synthesis, rendered in blocks of 128
//! frames at 48 kHz, paced as a sound card would call them, with 1, 2 and 4
//! threads and one per core.
//!
//! ```text
//! cargo run --release -p manguebyte-core --example pool_speed [partials] [blocks]
//! ```
//!
//! For each number of threads it prints the time `run` and the mix take
//! per block (mean, 99th percentile, worst), the speed-up of the mean over
//! one thread, the blocks with a missed job, and the wake latency: from the
//! start of `run` to the first job a worker begins. It also checks that
//! every block's mix is bit for bit the one-thread mix. Build it optimised:
//! the debug figures mean nothing. Other work on the machine shows in the
//! worst blocks.

use std::thread;
use std::time::{Duration, Instant};

use manguebyte_core::pool::{Job, PoolConfig, Promotion, RenderPool};

const JOBS: usize = 64;
const FRAMES: usize = 128;
const RATE: f64 = 48_000.0;

/// A voice of `partials` sines, each with its own slowly moving level.
struct Voice {
    phases: Vec<f64>,
    steps: Vec<f64>,
    levels: Vec<f32>,
    out: [f32; FRAMES],
    /// When this block's rendering began, and whether on a worker.
    started: Option<(Instant, bool)>,
}

impl Voice {
    fn new(index: usize, partials: usize) -> Voice {
        let root = 55.0 * 2f64.powf(index as f64 / 12.0);
        Voice {
            phases: vec![0.0; partials],
            steps: (1..=partials)
                .map(|partial| (root * partial as f64 / RATE).fract())
                .collect(),
            levels: (1..=partials).map(|partial| 0.1 / partial as f32).collect(),
            out: [0.0; FRAMES],
            started: None,
        }
    }
}

impl Job for Voice {
    fn render(&mut self) {
        let on_worker = thread::current()
            .name()
            .is_some_and(|name| name.starts_with("manguebyte-pool"));
        self.started = Some((Instant::now(), on_worker));
        self.out = [0.0; FRAMES];
        for ((phase, step), level) in self
            .phases
            .iter_mut()
            .zip(&self.steps)
            .zip(&mut self.levels)
        {
            for sample in &mut self.out {
                *sample += *level * (*phase * std::f64::consts::TAU).sin() as f32;
                *phase += step;
                if *phase >= 1.0 {
                    *phase -= 1.0;
                }
            }
            *level *= 0.999_99;
        }
    }
}

/// A hash of the mix's bits, to compare runs without keeping them.
fn hash(samples: &[f32]) -> u64 {
    samples.iter().fold(0xcbf2_9ce4_8422_2325, |hash, sample| {
        (hash ^ u64::from(sample.to_bits())).wrapping_mul(0x0100_0000_01b3)
    })
}

struct Figures {
    mean: Duration,
    p99: Duration,
    worst: Duration,
    overruns: u64,
    wake: Vec<Duration>,
    hashes: Vec<u64>,
    priorities: String,
}

fn measure(threads: usize, partials: usize, blocks: usize) -> Figures {
    let period = Duration::from_secs_f64(FRAMES as f64 / RATE);
    let voices = (0..JOBS).map(|index| Voice::new(index, partials)).collect();
    // The wait limit is the whole block: past it the block is late anyway.
    let mut pool = RenderPool::new(
        voices,
        PoolConfig::with_workers(threads - 1).wait_limit(period),
    );
    let priorities = format!("{:?}", pool.priorities());
    let mut mix = [0.0f32; FRAMES];
    let mut times = Vec::with_capacity(blocks);
    let mut wake = Vec::with_capacity(blocks);
    let mut hashes = Vec::with_capacity(blocks);
    let mut next = Instant::now();
    for block in 0..blocks + 50 {
        // Paced as a device would: one block per period, the workers
        // parked in between.
        next += period;
        let start = Instant::now();
        pool.run(JOBS);
        mix.fill(0.0);
        for (_, voice) in pool.rendered() {
            for (sum, sample) in mix.iter_mut().zip(&voice.out) {
                *sum += sample;
            }
        }
        let took = start.elapsed();
        if block >= 50 {
            times.push(took);
            hashes.push(hash(&mix));
            let first = (0..JOBS)
                .filter_map(|index| pool.job(index)?.started)
                .filter(|(_, on_worker)| *on_worker)
                .map(|(at, _)| at.saturating_duration_since(start))
                .min();
            wake.extend(first);
        }
        let now = Instant::now();
        if next > now {
            let left = next - now;
            if left > Duration::from_millis(2) {
                thread::sleep(left - Duration::from_millis(2));
            }
            while Instant::now() < next {
                thread::yield_now();
            }
        } else {
            next = now;
        }
    }
    let overruns = pool.stats().overruns;
    times.sort();
    wake.sort();
    Figures {
        mean: times.iter().sum::<Duration>() / times.len() as u32,
        p99: times[times.len() * 99 / 100],
        worst: *times.last().unwrap(),
        overruns,
        wake,
        hashes,
        priorities,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let partials = args.next().and_then(|arg| arg.parse().ok()).unwrap_or(32);
    let blocks = args
        .next()
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(3_000);
    let cores = thread::available_parallelism().map_or(1, usize::from);
    let audio = Promotion::current_thread();
    let period = FRAMES as f64 / RATE * 1e3;
    println!(
        "{JOBS} voices of {partials} partials, {FRAMES} frames at {RATE} Hz \
         (a block every {period:.3} ms), {blocks} blocks, {cores} cores"
    );
    println!("calling thread: {:?}", audio.priority());
    let mut counts = vec![1, 2, 4, cores];
    counts.retain(|&threads| threads <= cores);
    counts.dedup();
    let mut alone: Option<Figures> = None;
    for threads in counts {
        let figures = measure(threads, partials, blocks);
        let ms = |time: Duration| time.as_secs_f64() * 1e3;
        let us = |time: Duration| time.as_secs_f64() * 1e6;
        let speed_up = alone.as_ref().map_or(1.0, |alone| {
            alone.mean.as_secs_f64() / figures.mean.as_secs_f64()
        });
        let same = alone
            .as_ref()
            .is_none_or(|alone| alone.hashes == figures.hashes);
        println!(
            "{threads} thread(s): mean {:.3} ms, p99 {:.3} ms, worst {:.3} ms, \
             speed-up {speed_up:.2}, blocks with a miss {}, bit-identical to 1 thread: {same}",
            ms(figures.mean),
            ms(figures.p99),
            ms(figures.worst),
            figures.overruns,
        );
        if !figures.wake.is_empty() {
            let wake = &figures.wake;
            println!(
                "    wake latency: median {:.1} us, p99 {:.1} us, worst {:.1} us \
                 ({} blocks); workers {}",
                us(wake[wake.len() / 2]),
                us(wake[wake.len() * 99 / 100]),
                us(*wake.last().unwrap()),
                wake.len(),
                figures.priorities,
            );
        }
        assert!(same, "the mix differs from one thread's");
        if alone.is_none() {
            alone = Some(figures);
        }
    }
}
