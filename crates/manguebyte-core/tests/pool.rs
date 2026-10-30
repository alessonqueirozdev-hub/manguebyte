//! Proves what `manguebyte_core::pool` promises: output bit for bit equal
//! to one thread's, a wait bounded by the limit when a worker stalls,
//! offline rendering on the calling thread, helpers from another pool, and
//! workers that end when the pool is dropped and sleep while it is idle.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use manguebyte_core::pool::{Job, PoolConfig, Priority, RenderPool};

/// A small deterministic random number generator (xorshift64*).
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A voice of a few partials through a one-pole filter, so that its state
/// carries from block to block and any change in how it is run shows.
#[derive(Clone)]
struct Voice {
    phases: Vec<f64>,
    steps: Vec<f64>,
    level: f32,
    pole: f32,
    state: f32,
    frames: usize,
    out: Vec<f32>,
}

impl Voice {
    fn random(random: &mut Random, longest: usize) -> Voice {
        let partials = 1 + random.below(12);
        Voice {
            phases: (0..partials).map(|_| random.unit()).collect(),
            steps: (0..partials).map(|_| random.unit() * 0.05).collect(),
            level: random.unit() as f32,
            pole: 0.5 + 0.49 * random.unit() as f32,
            state: 0.0,
            frames: longest,
            out: vec![0.0; longest],
        }
    }
}

impl Job for Voice {
    fn render(&mut self) {
        for sample in &mut self.out[..self.frames] {
            let mut sum = 0.0f64;
            for (phase, step) in self.phases.iter_mut().zip(&self.steps) {
                sum += (*phase * std::f64::consts::TAU).sin();
                *phase = (*phase + step).fract();
            }
            self.state = self.state * self.pole + (1.0 - self.pole) * sum as f32;
            *sample = self.state * self.level;
        }
    }
}

/// Adds the first `frames` samples of each voice into `mix`, in order.
fn mix_into<'a>(mix: &mut [f32], frames: usize, voices: impl Iterator<Item = &'a Voice>) {
    mix[..frames].fill(0.0);
    for voice in voices {
        for (sum, sample) in mix[..frames].iter_mut().zip(&voice.out) {
            *sum += sample;
        }
    }
}

fn bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|sample| sample.to_bits()).collect()
}

#[test]
fn output_matches_one_thread_bit_for_bit() {
    let (threads, sizes, blocks): (Vec<usize>, &[usize], usize) = if cfg!(miri) {
        (vec![1, 3], &[16], 3)
    } else {
        ((1..=8).collect(), &[1, 7, 64, 128, 333], 12)
    };
    for &threads in &threads {
        for &longest in sizes {
            let mut random = Random(0x9e37_79b9_7f4a_7c15 ^ (threads * 1000 + longest) as u64);
            let jobs = 1 + random.below(if cfg!(miri) { 6 } else { 48 });
            let voices: Vec<Voice> = (0..jobs)
                .map(|_| Voice::random(&mut random, longest))
                .collect();
            let mut alone = voices.clone();
            let mut pool = RenderPool::new(
                voices,
                PoolConfig::with_workers(threads - 1).wait_limit(Duration::from_secs(10)),
            );
            let mut mix = vec![0.0f32; longest];
            let mut expected = vec![0.0f32; longest];
            for _ in 0..blocks {
                // The same changes to both: a block length, a level, and how
                // many of the voices play.
                let frames = 1 + random.below(longest);
                let changed = random.below(jobs);
                let level = random.unit() as f32;
                let count = random.below(jobs + 1);
                for (index, voice) in alone.iter_mut().enumerate() {
                    pool.job_mut(index).unwrap().frames = frames;
                    voice.frames = frames;
                }
                pool.job_mut(changed).unwrap().level = level;
                alone[changed].level = level;

                let report = pool.run(count);
                assert_eq!(report.missed, 0, "{threads} threads missed a job");
                assert_eq!(report.rendered, count);
                for voice in &mut alone[..count] {
                    voice.render();
                }
                mix_into(&mut mix, frames, pool.rendered().map(|(_, voice)| voice));
                mix_into(&mut expected, frames, alone[..count].iter());
                assert_eq!(
                    bits(&mix[..frames]),
                    bits(&expected[..frames]),
                    "{threads} threads, blocks of {longest}"
                );
                for (index, voice) in alone.iter().enumerate() {
                    let pooled = pool.job(index).unwrap();
                    assert_eq!(bits(&pooled.out), bits(&voice.out));
                    assert_eq!(pooled.state.to_bits(), voice.state.to_bits());
                }
            }
            assert_eq!(pool.stats().overruns, 0);
        }
    }
}

/// Records the thread that rendered it.
struct Where {
    thread: Option<ThreadId>,
    name: Option<String>,
}

impl Job for Where {
    fn render(&mut self) {
        let current = thread::current();
        self.thread = Some(current.id());
        self.name = current.name().map(str::to_owned);
    }
}

fn wheres(count: usize) -> Vec<Where> {
    (0..count)
        .map(|_| Where {
            thread: None,
            name: None,
        })
        .collect()
}

#[test]
fn an_offline_pool_renders_on_the_calling_thread() {
    let mut pool = RenderPool::new(wheres(16), PoolConfig::offline());
    assert_eq!(pool.workers(), 0);
    assert!(pool.priorities().is_empty());
    assert!(pool.run(16).is_complete());
    for (_, job) in pool.rendered() {
        assert_eq!(job.thread, Some(thread::current().id()));
    }
    assert_eq!(pool.rendered().count(), 16);
}

#[test]
fn every_worker_reports_its_priority() {
    let pool = RenderPool::new(wheres(1), PoolConfig::with_workers(3));
    assert_eq!(pool.workers(), 3);
    assert_eq!(pool.priorities().len(), 3);
    for priority in pool.priorities() {
        if cfg!(miri) {
            assert_eq!(*priority, Priority::Normal);
        } else if cfg!(windows) {
            // MMCSS needs no rights, but its service can be turned off.
            assert!(matches!(priority, Priority::ProAudio | Priority::Refused));
        } else {
            assert_ne!(*priority, Priority::Normal);
        }
    }
    let plain = RenderPool::new(wheres(1), PoolConfig::with_workers(2).real_time(false));
    assert_eq!(plain.priorities(), [Priority::Normal, Priority::Normal]);
}

#[test]
fn helpers_of_another_pool_render_jobs() {
    let mut pool = RenderPool::new(wheres(32), PoolConfig::with_workers(2));
    // A first block, so that the workers have seen a block and parked.
    assert!(pool.run(32).is_complete());
    thread::sleep(Duration::from_millis(50));
    let helper = pool.helper();
    let mut asked = None;
    let report = pool.run_with(32, |helpers| {
        asked = Some(helpers);
        // As a host's `request_exec` does: run the tasks, return when done.
        thread::scope(|scope| {
            for task in 0..3 {
                let helper = helper.clone();
                thread::Builder::new()
                    .name(format!("host-{task}"))
                    .spawn_scoped(scope, move || helper.help())
                    .unwrap();
            }
        });
        true
    });
    assert_eq!(asked, Some(31));
    assert!(report.is_complete());
    for (_, job) in pool.rendered() {
        let name = job.name.as_deref().unwrap_or("");
        assert!(!name.starts_with("manguebyte-pool"), "a worker was woken");
    }
    // The host's threads took the first jobs while the audio thread waited
    // in `request`.
    let by_host = pool
        .rendered()
        .filter(|(_, job)| {
            job.name
                .as_deref()
                .is_some_and(|name| name.starts_with("host-"))
        })
        .count();
    assert_eq!(by_host, 32);

    // A host that declines leaves the work to the pool's own threads.
    let report = pool.run_with(32, |_| false);
    assert!(report.is_complete());
}

/// What a stalled worker and the audio thread share in the stall test.
#[derive(Default)]
struct Gate {
    /// The next job a worker takes stalls, once.
    armed: AtomicBool,
    /// A worker has taken a job and is stalling.
    stalling: AtomicBool,
    /// The stall is over.
    over: AtomicBool,
}

struct Stall {
    gate: Arc<Gate>,
    renders: u64,
}

const STALL: Duration = Duration::from_millis(400);

impl Job for Stall {
    fn render(&mut self) {
        let on_worker = thread::current()
            .name()
            .is_some_and(|name| name.starts_with("manguebyte-pool"));
        if on_worker {
            if self.gate.armed.swap(false, Ordering::SeqCst) {
                self.gate.stalling.store(true, Ordering::SeqCst);
                // The test hook: a worker the system has stopped.
                thread::sleep(STALL);
                self.gate.over.store(true, Ordering::SeqCst);
            }
        } else {
            // The audio thread gives the worker time to wake and take a
            // job, so that the stall does happen.
            let start = Instant::now();
            while !self.gate.stalling.load(Ordering::SeqCst)
                && start.elapsed() < Duration::from_secs(5)
            {
                thread::yield_now();
            }
        }
        self.renders += 1;
    }
}

#[test]
fn a_stalled_worker_costs_the_audio_thread_at_most_the_wait_limit() {
    let gate = Arc::new(Gate::default());
    gate.armed.store(true, Ordering::SeqCst);
    let jobs = (0..8)
        .map(|_| Stall {
            gate: Arc::clone(&gate),
            renders: 0,
        })
        .collect();
    let limit = Duration::from_millis(2);
    let mut pool = RenderPool::new(jobs, PoolConfig::with_workers(1).wait_limit(limit));

    let start = Instant::now();
    let report = pool.run(8);
    let first = start.elapsed();
    assert!(gate.stalling.load(Ordering::SeqCst));
    assert!(
        !gate.over.load(Ordering::SeqCst),
        "the stall ended too soon to test"
    );
    assert_eq!(report.missed, 1, "{report:?}");
    assert_eq!(report.rendered, 7);
    let stalled = (0..8).find(|&index| !pool.was_rendered(index)).unwrap();
    // Not lent out while the worker holds it.
    assert!(pool.job(stalled).is_none());
    assert!(pool.job_mut(stalled).is_none());
    assert_eq!(pool.rendered().count(), 7);

    // The next block runs while the worker still sleeps: the job is
    // missed again, the others render, and the wait is still bounded.
    let start = Instant::now();
    let report = pool.run(8);
    let second = start.elapsed();
    assert!(
        !gate.over.load(Ordering::SeqCst),
        "the stall ended too soon to test"
    );
    assert_eq!(
        report,
        manguebyte_core::pool::BlockReport {
            rendered: 7,
            missed: 1
        }
    );
    assert!(!pool.was_rendered(stalled));
    // Far below the stall; generous for a loaded machine.
    assert!(first < STALL / 2, "first block took {first:?}");
    assert!(second < STALL / 4, "second block took {second:?}");

    // Once the worker is done, the job is the pool's again, and has been
    // rendered exactly once per block it was not missed in, plus the one
    // the worker finished late.
    let start = Instant::now();
    while pool.job(stalled).is_none() && start.elapsed() < Duration::from_secs(10) {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(gate.over.load(Ordering::SeqCst));
    assert_eq!(pool.job(stalled).unwrap().renders, 1);
    pool.set_wait_limit(Duration::from_secs(10));
    assert!(pool.run(8).is_complete());
    assert_eq!(pool.job(stalled).unwrap().renders, 2);
    for index in (0..8).filter(|&index| index != stalled) {
        assert_eq!(pool.job(index).unwrap().renders, 3);
    }
    let stats = pool.stats();
    assert_eq!((stats.blocks, stats.overruns, stats.missed_jobs), (3, 2, 2));
}

/// Holds a token, so that the test sees when the pool's jobs are freed.
struct Token(#[allow(dead_code)] Arc<()>);

impl Job for Token {
    fn render(&mut self) {}
}

#[test]
fn dropping_the_pool_ends_its_workers() {
    let token = Arc::new(());
    let mut pool = RenderPool::new(
        (0..4).map(|_| Token(Arc::clone(&token))).collect(),
        PoolConfig::with_workers(4),
    );
    pool.run(4);
    assert_eq!(Arc::strong_count(&token), 5);
    drop(pool);
    // Every worker held the jobs; they are freed, so every worker ended.
    assert_eq!(Arc::strong_count(&token), 1);
}

/// Counts the threads that rendered it, to see workers busy at once.
struct Light(Arc<AtomicUsize>);

impl Job for Light {
    fn render(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn idle_workers_sleep() {
    let renders = Arc::new(AtomicUsize::new(0));
    let mut pool = RenderPool::new(
        (0..16).map(|_| Light(Arc::clone(&renders))).collect(),
        PoolConfig::with_workers(4),
    );
    for _ in 0..20 {
        pool.run(16);
    }
    assert_eq!(renders.load(Ordering::Relaxed), 320);
    // Let the workers finish looking at the last block and park.
    thread::sleep(Duration::from_millis(50));
    let before = pool.stats().worker_wakeups;
    thread::sleep(Duration::from_millis(if cfg!(miri) { 20 } else { 300 }));
    let woken = pool.stats().worker_wakeups - before;
    // A parked thread may wake spuriously, but rarely.
    assert!(woken <= 4, "idle workers woke {woken} times");
}
