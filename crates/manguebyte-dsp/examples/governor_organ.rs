//! Holds a synthetic pipe organ, asked for twice what the machine can
//! render, under its target load with `manguebyte_core::governor`, on the
//! real clock.
//!
//! ```text
//! cargo run --release -p manguebyte-dsp --example governor_organ \
//!     [seconds] [factor] [sleep|spin|sleep-rt|spin-rt] [spike_blocks] [max_rise]
//! ```
//!
//! Each pipe is a [`PartialBank`] of 32 partials, whose levels fall with
//! their number; the pipes' levels differ. The cost model counts partials
//! as rendered, in the bank's groups of 16, so a pipe costs 32 units at its
//! full tier and 16 at its cheap one (the 16 quietest partials dropped).
//! Each block the organ ranks its pipes by level and spends the budget
//! from the loudest down: the full tier while it fits, then the cheap
//! tier, then nothing (the pipe is stolen for that block).
//!
//! First the example times the organ and works out how many pipes fill a
//! block of 128 frames at 48 kHz (2.67 ms), twice over, since a pipe costs
//! more once the organ outgrows the processor's caches. Then it draws
//! `factor` (2 unless given) times that many and plays them for `seconds`
//! (20 unless given), one block per deadline, sleeping until the next one
//! as a sound card's callback would wait, or spinning (`spin`), which keeps
//! the thread on its core; `spike_blocks` and `max_rise` set the
//! governor's (1 and 2 unless given). It prints, after the first second,
//! the measured load's percentiles, the blocks over the target and over
//! the deadline, and the share of the asked-for partials rendered, beside a
//! second without the governor. The thread has the system's normal
//! priority, or asks for real-time scheduling with `sleep-rt` or `spin-rt`
//! (`manguebyte_core::pool::Promotion`: MMCSS "Pro Audio" on Windows).
//! Build it optimised: the debug figures mean nothing. Other work on the
//! machine shows in the worst blocks, and in how much the governor lets
//! the organ render.

use std::hint::black_box;
use std::thread;
use std::time::{Duration, Instant};

use manguebyte_core::governor::{Allowance, Governor, GovernorConfig};
use manguebyte_core::pool::{Priority, Promotion};
use manguebyte_dsp::{Partial, PartialBank};

const RATE: f64 = 48_000.0;
const FRAMES: usize = 128;
const PARTIALS: usize = 32;
const FULL: f64 = 32.0;
const CHEAP: f64 = 16.0;

/// A pipe: the same partials in two banks, all of them for the full tier
/// and the loudest half for the cheap one, so that changing tier forgets
/// nothing.
struct Pipe {
    full: PartialBank,
    cheap: PartialBank,
    level: f32,
}

impl Pipe {
    fn new(index: usize) -> Pipe {
        let root = 32.7 * 2f64.powf((index % 61) as f64 / 12.0);
        // A level of its own for each pipe, from 0.1 to 1, fixed.
        let level = 0.1 + 0.9 * ((index * 37) % 100) as f32 / 100.0;
        let bank = |partials: usize| {
            let mut bank = PartialBank::new(partials);
            bank.set_sample_rate(RATE);
            bank.set_len(partials);
            for partial in 0..partials {
                let number = partial + 1;
                bank.set(
                    partial,
                    Partial {
                        freq_hz: (root * number as f64).min(RATE * 0.45),
                        gain: level / number as f32 / 64.0,
                        pan: ((index % 7) as f32 - 3.0) / 3.0,
                    },
                );
            }
            bank
        };
        Pipe {
            full: bank(PARTIALS),
            cheap: bank(PARTIALS / 2),
            level,
        }
    }
}

struct Organ {
    pipes: Vec<Pipe>,
    /// Pipe numbers, loudest first.
    order: Vec<usize>,
    left: Vec<f32>,
    right: Vec<f32>,
}

impl Organ {
    fn new(pipes: usize) -> Organ {
        let pipes: Vec<Pipe> = (0..pipes).map(Pipe::new).collect();
        let mut order: Vec<usize> = (0..pipes.len()).collect();
        order.sort_by(|&a, &b| pipes[b].level.total_cmp(&pipes[a].level));
        Organ {
            pipes,
            order,
            left: vec![0.0; FRAMES],
            right: vec![0.0; FRAMES],
        }
    }

    fn demand(&self) -> f64 {
        self.pipes.len() as f64 * FULL
    }

    /// Renders one block within `budget` units and returns the units spent
    /// and whether anything was cut.
    fn render(&mut self, budget: f64) -> (f64, bool) {
        self.left.fill(0.0);
        self.right.fill(0.0);
        let mut allowance = Allowance::new(budget);
        for &index in &self.order {
            let pipe = &mut self.pipes[index];
            if allowance.try_take(FULL) {
                pipe.full.render_add(&mut self.left, &mut self.right);
            } else if allowance.try_take(CHEAP) {
                pipe.cheap.render_add(&mut self.left, &mut self.right);
            }
        }
        black_box((&self.left, &self.right));
        (allowance.spent(), allowance.limited())
    }
}

fn deadline() -> Duration {
    Duration::from_secs_f64(FRAMES as f64 / RATE)
}

/// How long one pipe at its full tier takes in a block, in an organ of
/// `pipes`: the median over many blocks, divided by the pipes.
fn time_per_pipe(pipes: usize) -> Duration {
    let mut organ = Organ::new(pipes);
    let mut times: Vec<Duration> = (0..200)
        .map(|_| {
            let start = Instant::now();
            organ.render(f64::INFINITY);
            start.elapsed()
        })
        .collect();
    times.sort();
    times[times.len() / 2] / pipes as u32
}

/// How the blocks are paced.
#[derive(Clone, Copy, PartialEq)]
enum Pacing {
    /// Sleeping until the next deadline, as a callback waits for the
    /// device.
    Sleep,
    /// Spinning until it, which keeps the thread on its core.
    Spin,
}

struct Report {
    loads: Vec<f64>,
    spent: Vec<f64>,
}

/// Plays `blocks` blocks paced on the deadline, with or without the
/// governor.
fn play(
    organ: &mut Organ,
    governor: &mut Governor,
    blocks: usize,
    governed: bool,
    pacing: Pacing,
) -> Report {
    let mut report = Report {
        loads: Vec::with_capacity(blocks),
        spent: Vec::with_capacity(blocks),
    };
    let start = Instant::now();
    for block in 0..blocks {
        let due = start + deadline() * block as u32;
        let now = Instant::now();
        if due > now {
            match pacing {
                Pacing::Sleep => thread::sleep(due - now),
                Pacing::Spin => {
                    while Instant::now() < due {
                        std::hint::spin_loop();
                    }
                }
            }
        }
        let budget = if governed {
            governor.budget()
        } else {
            f64::INFINITY
        };
        let begun = Instant::now();
        governor.begin_block();
        let (spent, limited) = organ.render(budget);
        governor.end_block(FRAMES, spent, limited);
        report
            .loads
            .push(begun.elapsed().as_secs_f64() / deadline().as_secs_f64());
        report.spent.push(spent);
    }
    report
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * fraction).round() as usize]
}

fn print(name: &str, report: &Report, skip: usize, demand: f64, target: f64) {
    let mut loads = report.loads[skip..].to_vec();
    loads.sort_by(f64::total_cmp);
    let count = loads.len() as f64;
    let over_target = loads.iter().filter(|&&load| load > target).count();
    let over_deadline = loads.iter().filter(|&&load| load > 1.0).count();
    let rendered = report.spent[skip..].iter().sum::<f64>() / count / demand;
    println!(
        "{name:<12} load p50 {:>5.1} %  p90 {:>5.1} %  p99 {:>6.1} %  max {:>7.1} %  \
         over target {:>5.2} %  over deadline {:>5.2} %  rendered {:>5.1} %",
        100.0 * percentile(&loads, 0.5),
        100.0 * percentile(&loads, 0.9),
        100.0 * percentile(&loads, 0.99),
        100.0 * loads[loads.len() - 1],
        100.0 * over_target as f64 / count,
        100.0 * over_deadline as f64 / count,
        100.0 * rendered,
    );
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let seconds: f64 = arguments
        .next()
        .map_or(20.0, |s| s.parse().expect("seconds"));
    let factor: f64 = arguments.next().map_or(2.0, |s| s.parse().expect("factor"));
    let pacing_name = arguments.next().unwrap_or_default();
    let pacing = if pacing_name.starts_with("spin") {
        Pacing::Spin
    } else {
        Pacing::Sleep
    };
    // Held to the end, so the whole run is at that priority.
    let promotion = pacing_name.ends_with("-rt").then(Promotion::current_thread);
    let mut config = GovernorConfig::default();
    if let Some(blocks) = arguments.next() {
        config = config.spike_blocks(blocks.parse().expect("spike blocks"));
    }
    if let Some(rise) = arguments.next() {
        config = config.max_rise(rise.parse().expect("max rise"));
    }

    // Twice over: first in a small organ, then at the size that gives.
    let mut pipes = 64;
    let mut per_pipe = Duration::ZERO;
    for _ in 0..2 {
        per_pipe = time_per_pipe(pipes);
        pipes = (factor * deadline().as_secs_f64() / per_pipe.as_secs_f64()).ceil() as usize;
    }
    println!(
        "a pipe of {PARTIALS} partials takes {:.2} µs a block of {FRAMES} frames in an organ \
         this size; drawing {pipes}, {factor} times what fills the {:.2} ms deadline; \
         pacing by {}, at {:?} priority",
        per_pipe.as_secs_f64() * 1e6,
        deadline().as_secs_f64() * 1e3,
        if pacing == Pacing::Spin {
            "spinning"
        } else {
            "sleeping"
        },
        promotion
            .as_ref()
            .map_or(Priority::Normal, Promotion::priority),
    );

    let mut organ = Organ::new(pipes);
    let mut governor = Governor::new(config);
    let target = governor.target();
    governor.set_sample_rate(RATE);
    governor.set_max_budget(organ.demand());
    let blocks_per_second = (RATE / FRAMES as f64).round() as usize;

    let free = play(&mut organ, &mut governor, blocks_per_second, false, pacing);
    governor.reset();
    let governed = play(
        &mut organ,
        &mut governor,
        (seconds * blocks_per_second as f64) as usize,
        true,
        pacing,
    );
    let demand = organ.demand();
    print("no governor", &free, 0, demand, target);
    print("governor", &governed, blocks_per_second, demand, target);
    let stats = governor.stats();
    println!(
        "governor: {} blocks, {} over target, {} over the deadline, {} cuts; final budget \
         {:.0} of {demand:.0} units ({:.1} %; at the calibrated cost, {:.1} % would load \
         the target)",
        stats.blocks,
        stats.over_target,
        stats.over_deadline,
        stats.cuts,
        governor.budget(),
        100.0 * governor.budget() / demand,
        100.0 * target / factor,
    );
}
