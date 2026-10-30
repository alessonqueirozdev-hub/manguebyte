//! Rendering independent jobs on several cores inside one audio callback.
//!
//! A [`RenderPool`] owns a fixed set of [`Job`]s (voices, divisions,
//! effect chains, each with its own output buffer) and a fixed number of
//! worker threads made when the pool is made. Each block, the audio thread
//! calls [`RenderPool::run`]: the workers and the audio thread itself take
//! jobs from a counter shared through atomics, each job renders into its own
//! memory, and once `run` returns the caller mixes the results in index
//! order through [`RenderPool::rendered`]. Because a job is rendered whole
//! by one thread and the sum is taken in one fixed order, the output is bit
//! for bit what rendering the same jobs one after another on one thread
//! gives, whatever the number of threads.
//!
//! ```
//! use manguebyte_core::pool::{Job, PoolConfig, RenderPool};
//!
//! struct Voice {
//!     level: f32,
//!     out: Vec<f32>,
//! }
//!
//! impl Job for Voice {
//!     fn render(&mut self) {
//!         self.out.fill(self.level);
//!     }
//! }
//!
//! let voices = (0..8)
//!     .map(|index| Voice { level: index as f32, out: vec![0.0; 64] })
//!     .collect();
//! let mut pool = RenderPool::new(voices, PoolConfig::with_workers(2));
//!
//! // On the audio thread, every block:
//! let report = pool.run(8);
//! let mut mix = [0.0f32; 64];
//! for (_, voice) in pool.rendered() {
//!     for (sum, sample) in mix.iter_mut().zip(&voice.out) {
//!         *sum += sample;
//!     }
//! }
//! assert!(report.is_complete());
//! assert_eq!(mix[0], 28.0);
//! ```
//!
//! # On the audio thread
//!
//! `run` does not allocate, lock or make system calls other than waking
//! parked workers with [`std::thread::Thread::unpark`], which is a system
//! call only when the worker is asleep: `WakeByAddressSingle` on Windows, a
//! futex wake on Linux, Android and FreeBSD, `dispatch_semaphore_signal` on
//! macOS. It reads the clock ([`Instant::now`]: `QueryPerformanceCounter`,
//! the vDSO's `clock_gettime` or `mach_absolute_time`, none of them a
//! system call) only while it waits for a job another thread holds.
//!
//! The audio thread is never idle while a job is waiting to be taken: it
//! takes jobs like a worker, and then renders itself any job no thread has
//! begun, even one whose turn a worker had drawn but not yet started. It
//! waits only for jobs a worker is rendering, by spinning, and never longer
//! than the pool's [wait limit](RenderPool::set_wait_limit).
//!
//! # When a worker is late
//!
//! A worker the system stops in the middle of a job (preempted, or paged
//! out) may still hold it when the wait limit passes. `run` then returns
//! anyway: that job's block is dropped from the mix (it is not in
//! [`rendered`](RenderPool::rendered)), [`BlockReport::missed`] counts it
//! and [`PoolStats::overruns`] counts the block. The worker finishes the job
//! when it runs again, so the job's own state (an oscillator's phase, an
//! envelope) moves on by one block whose sound is lost; until it finishes,
//! [`job_mut`](RenderPool::job_mut) returns `None` for it and later blocks
//! count it missed too. Nothing is ever mixed half-rendered, no two threads
//! touch one job at a time, and the audio thread never waits unboundedly.
//!
//! This is why a pool owns its jobs, and why a job borrows nothing: a job
//! may still be running after `run` has returned.
//!
//! # Offline
//!
//! A pool of zero workers ([`PoolConfig::offline`]) renders every job on
//! the calling thread, in index order, through the same calls.

mod priority;

use std::cell::UnsafeCell;
use std::hint;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub use priority::{Priority, Promotion};

/// One piece of independent work a [`RenderPool`] renders each block: a
/// voice, a division of an organ, a chain of effects.
///
/// A job keeps everything it reads and writes: its input, set through
/// [`RenderPool::job_mut`] between blocks, and its output buffer, read
/// through [`RenderPool::rendered`] after `run`. Its rendering must not
/// allocate, lock or wait, since it runs on the audio thread or on a worker
/// the audio thread is waiting for.
pub trait Job: Send + 'static {
    /// Renders one block.
    fn render(&mut self);
}

/// How a [`RenderPool`] is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolConfig {
    workers: usize,
    real_time: bool,
    wait_limit: Duration,
}

impl PoolConfig {
    /// The wait limit a pool starts with.
    pub const DEFAULT_WAIT_LIMIT: Duration = Duration::from_millis(1);

    /// A pool with this many worker threads beside the audio thread.
    pub fn with_workers(workers: usize) -> PoolConfig {
        PoolConfig {
            workers,
            real_time: true,
            wait_limit: Self::DEFAULT_WAIT_LIMIT,
        }
    }

    /// A worker for every core but the one the audio thread runs on (none
    /// where the system cannot say how many cores it has).
    pub fn all_cores() -> PoolConfig {
        let cores = thread::available_parallelism().map_or(1, usize::from);
        Self::with_workers(cores - 1)
    }

    /// No workers: everything renders on the thread that calls `run`.
    pub fn offline() -> PoolConfig {
        Self::with_workers(0)
    }

    /// Whether the workers ask for real-time scheduling (on by default).
    /// See [`Promotion`].
    pub fn real_time(mut self, real_time: bool) -> PoolConfig {
        self.real_time = real_time;
        self
    }

    /// How long `run` may wait for a job a worker holds. See
    /// [`RenderPool::set_wait_limit`].
    pub fn wait_limit(mut self, limit: Duration) -> PoolConfig {
        self.wait_limit = limit;
        self
    }

    /// The number of workers asked for.
    pub fn workers(&self) -> usize {
        self.workers
    }
}

/// What one call to [`RenderPool::run`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlockReport {
    /// Jobs rendered for this block.
    pub rendered: usize,
    /// Jobs asked for but not rendered in time: a worker still held them
    /// when the wait limit passed.
    pub missed: usize,
}

impl BlockReport {
    /// Whether every job asked for was rendered.
    pub fn is_complete(&self) -> bool {
        self.missed == 0
    }
}

/// Counts kept over a pool's life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Calls to `run`.
    pub blocks: u64,
    /// Blocks in which at least one job was missed.
    pub overruns: u64,
    /// Jobs missed, over every block.
    pub missed_jobs: u64,
    /// Times a worker woke from parking, whether for work or not. It stays
    /// still while the pool is idle.
    pub worker_wakeups: u64,
}

/// Keeps what it holds on a cache line of its own.
#[repr(align(64))]
struct CachePadded<T>(T);

struct Slot<J> {
    /// Held by the one thread that may touch `job`.
    busy: AtomicBool,
    /// The last block this job was rendered for; 0 for none.
    rendered: AtomicU64,
    job: UnsafeCell<J>,
}

struct Shared<J> {
    slots: Box<[CachePadded<Slot<J>>]>,
    /// The block being rendered, counted from 1.
    block: CachePadded<AtomicU64>,
    /// The last block `run` has finished with.
    closed: CachePadded<AtomicU64>,
    /// The next job to hand out in the current block.
    next: CachePadded<AtomicUsize>,
    /// How many jobs the current block renders.
    count: AtomicUsize,
    shutdown: AtomicBool,
    wakeups: AtomicU64,
}

// SAFETY: a job is only touched by the thread holding its slot's `busy`
// flag, taken with an atomic swap and given back with a Release store, so
// jobs move between threads (which needs `J: Send`) but are never shared.
// The audio thread lends a job out (`job`, `job_mut`, `rendered`) only when
// no block is open, and a thread that takes the flag while no block is open
// gives it back without touching the job (see `try_render`).
unsafe impl<J: Send> Sync for Shared<J> {}
// SAFETY: as above.
unsafe impl<J: Send> Send for Shared<J> {}

/// Gives a slot's flag back, also if the job panics.
struct Hold<'a>(&'a AtomicBool);

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl<J: Job> Shared<J> {
    /// Renders job `index` for `block` if this thread can take it and it is
    /// still due. Returns whether this call rendered it.
    fn try_render(&self, index: usize, block: u64) -> bool {
        let slot = &self.slots[index].0;
        if slot.busy.load(Ordering::Relaxed) || slot.busy.swap(true, Ordering::SeqCst) {
            return false;
        }
        let hold = Hold(&slot.busy);
        // The flag and `closed` are a pair: `close` stores `closed` and the
        // audio thread then reads the flag before lending a job out, both
        // sequentially consistent, so either this load sees the block
        // closed or the audio thread sees the flag held.
        if self.closed.0.load(Ordering::SeqCst) >= block
            || slot.rendered.load(Ordering::Relaxed) >= block
        {
            return false;
        }
        // SAFETY: this thread holds the slot's flag (see `Shared`'s `Sync`).
        unsafe { (*slot.job.get()).render() };
        slot.rendered.store(block, Ordering::Release);
        drop(hold);
        true
    }

    /// Takes jobs of `block` until none are left to take.
    fn take_jobs(&self, block: u64) {
        let count = self.count.load(Ordering::Relaxed);
        while self.closed.0.load(Ordering::Acquire) < block {
            let index = self.next.0.fetch_add(1, Ordering::AcqRel);
            if index >= count {
                break;
            }
            self.try_render(index, block);
        }
    }
}

/// Renders a fixed set of jobs each block on a fixed set of threads. See
/// the [module documentation](self).
pub struct RenderPool<J: Job> {
    shared: Arc<Shared<J>>,
    workers: Vec<JoinHandle<()>>,
    priorities: Vec<Priority>,
    /// Whether each job was rendered in the last block.
    done: Box<[bool]>,
    block: u64,
    wait_limit: Duration,
    stats: PoolStats,
}

impl<J: Job> RenderPool<J> {
    /// Makes a pool owning `jobs`, and starts its workers. Call it off the
    /// audio thread (in `prepare`): it allocates and starts threads, and
    /// waits until each worker has asked for its priority.
    ///
    /// A worker the system cannot start is left out;
    /// [`workers`](Self::workers) says how many run. On WebAssembly
    /// without threads there are none.
    pub fn new(jobs: Vec<J>, config: PoolConfig) -> RenderPool<J> {
        let done = vec![false; jobs.len()].into_boxed_slice();
        let slots = jobs
            .into_iter()
            .map(|job| {
                CachePadded(Slot {
                    busy: AtomicBool::new(false),
                    rendered: AtomicU64::new(0),
                    job: UnsafeCell::new(job),
                })
            })
            .collect();
        let shared = Arc::new(Shared {
            slots,
            block: CachePadded(AtomicU64::new(0)),
            closed: CachePadded(AtomicU64::new(0)),
            next: CachePadded(AtomicUsize::new(0)),
            count: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            wakeups: AtomicU64::new(0),
        });
        let (report, reports) = mpsc::channel();
        let mut workers = Vec::with_capacity(config.workers);
        for index in 0..config.workers {
            let shared = Arc::clone(&shared);
            let report = report.clone();
            let real_time = config.real_time;
            let started = thread::Builder::new()
                .name(format!("manguebyte-pool-{index}"))
                .spawn(move || work(&shared, real_time, report));
            match started {
                Ok(handle) => workers.push(handle),
                Err(_) => break,
            }
        }
        drop(report);
        let priorities = workers
            .iter()
            .map(|_| reports.recv().unwrap_or(Priority::Normal))
            .collect();
        RenderPool {
            shared,
            workers,
            priorities,
            done,
            block: 0,
            wait_limit: config.wait_limit,
            stats: PoolStats::default(),
        }
    }

    /// How many jobs the pool owns.
    pub fn len(&self) -> usize {
        self.shared.slots.len()
    }

    /// Whether the pool owns no jobs.
    pub fn is_empty(&self) -> bool {
        self.shared.slots.is_empty()
    }

    /// How many worker threads run beside the audio thread.
    pub fn workers(&self) -> usize {
        self.workers.len()
    }

    /// The priority each worker obtained, in the order they were started.
    pub fn priorities(&self) -> &[Priority] {
        &self.priorities
    }

    /// How long `run` may wait for a job a worker is rendering.
    pub fn wait_limit(&self) -> Duration {
        self.wait_limit
    }

    /// Sets how long `run` may spin waiting for a job a worker is still
    /// rendering once every other job is done. Past it, `run` returns
    /// without that job (see the [module documentation](self)). Choose it
    /// from the block's budget in `prepare`: the time a block may take,
    /// less what the work after `run` needs.
    pub fn set_wait_limit(&mut self, limit: Duration) {
        self.wait_limit = limit;
    }

    /// Job `index`, or `None` if it does not exist or a late worker still
    /// renders it.
    pub fn job(&self, index: usize) -> Option<&J> {
        let slot = &self.shared.slots.get(index)?.0;
        if slot.busy.load(Ordering::SeqCst) {
            return None;
        }
        // SAFETY: no block is open (`run` takes `&mut self` and closes its
        // block before returning) and no thread holds the flag, so nothing
        // writes the job: a thread that takes the flag now finds the block
        // closed and leaves the job alone.
        Some(unsafe { &*slot.job.get() })
    }

    /// Job `index` to change between blocks, or `None` if it does not
    /// exist or a late worker still renders it (only after a missed block).
    pub fn job_mut(&mut self, index: usize) -> Option<&mut J> {
        let slot = &self.shared.slots.get(index)?.0;
        if slot.busy.load(Ordering::SeqCst) {
            return None;
        }
        // SAFETY: as in `job`; `&mut self` keeps every other borrow out.
        Some(unsafe { &mut *slot.job.get() })
    }

    /// Whether job `index` was rendered in the last block.
    pub fn was_rendered(&self, index: usize) -> bool {
        self.done.get(index).copied().unwrap_or(false)
    }

    /// The jobs rendered in the last block, in index order: the order to
    /// mix them in for the output to match a single thread's.
    pub fn rendered(&self) -> impl Iterator<Item = (usize, &J)> {
        self.done
            .iter()
            .enumerate()
            .filter(|(_, done)| **done)
            .map(|(index, _)| {
                // SAFETY: the job finished rendering for the last block,
                // which is closed, so no thread writes it any more (a
                // thread that takes its flag now leaves it alone).
                (index, unsafe { &*self.shared.slots[index].0.job.get() })
            })
    }

    /// Counts kept since the pool was made.
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            worker_wakeups: self.shared.wakeups.load(Ordering::Relaxed),
            ..self.stats
        }
    }

    /// A handle through which other threads, such as a host's thread pool,
    /// can render jobs of the block being run. See
    /// [`run_with`](Self::run_with).
    pub fn helper(&self) -> Helper<J> {
        Helper {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Renders the first `count` jobs (all of them if `count` is larger)
    /// for one block, on the workers and on the calling thread, and returns
    /// once they are done or the wait limit has passed. Call it on the
    /// audio thread; it does not allocate or lock.
    pub fn run(&mut self, count: usize) -> BlockReport {
        self.run_with(count, |_| false)
    }

    /// As [`run`](Self::run), but first offers the work to other threads:
    /// `request` is called with the number of helpers that could be used
    /// (one less than the jobs), and returns `true` if it had that many
    /// calls of [`Helper::help`] made, and they have returned, as a CLAP
    /// host's `request_exec` does. Then the pool's own workers are not
    /// woken. If it returns `false` they are.
    pub fn run_with(&mut self, count: usize, request: impl FnOnce(usize) -> bool) -> BlockReport {
        let shared = &*self.shared;
        let count = count.min(shared.slots.len());
        self.block += 1;
        let block = self.block;
        shared.count.store(count, Ordering::Relaxed);
        shared.next.0.store(0, Ordering::Release);
        shared.block.0.store(block, Ordering::Release);

        let helpers = count.saturating_sub(1);
        if helpers > 0 && !request(helpers) {
            for worker in self.workers.iter().take(helpers) {
                worker.thread().unpark();
            }
        }

        shared.take_jobs(block);

        // Every job has been handed out. Render any no thread has begun,
        // and wait, up to the limit, for those another thread renders.
        let mut deadline = None;
        let mut first = 0;
        loop {
            let mut pending = None;
            for index in first..count {
                let rendered = shared.slots[index].0.rendered.load(Ordering::Acquire);
                if rendered != block && !shared.try_render(index, block) && pending.is_none() {
                    pending = Some(index);
                }
            }
            let Some(index) = pending else { break };
            first = index;
            let deadline = *deadline.get_or_insert_with(|| Instant::now() + self.wait_limit);
            if Instant::now() >= deadline {
                break;
            }
            hint::spin_loop();
        }

        // Close the block: from here no thread starts a job of it.
        shared.closed.0.store(block, Ordering::SeqCst);
        let mut report = BlockReport::default();
        for (index, done) in self.done.iter_mut().enumerate() {
            *done =
                index < count && shared.slots[index].0.rendered.load(Ordering::Acquire) == block;
            if *done {
                report.rendered += 1;
            }
        }
        report.missed = count - report.rendered;
        self.stats.blocks += 1;
        if report.missed > 0 {
            self.stats.overruns += 1;
            self.stats.missed_jobs += report.missed as u64;
        }
        report
    }
}

impl<J: Job> Drop for RenderPool<J> {
    /// Stops the workers and waits for them to end, including a late one
    /// finishing its job. Drop a pool off the audio thread.
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        for worker in &self.workers {
            worker.thread().unpark();
        }
        for worker in self.workers.drain(..) {
            // A worker that panicked in a job has ended already.
            let _ = worker.join();
        }
    }
}

impl<J: Job> std::fmt::Debug for RenderPool<J> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RenderPool")
            .field("jobs", &self.len())
            .field("workers", &self.workers())
            .field("priorities", &self.priorities)
            .field("wait_limit", &self.wait_limit)
            .field("stats", &self.stats())
            .finish()
    }
}

/// Lets another thread render jobs of the block a [`RenderPool`] is
/// running. It can be cloned and sent anywhere; it keeps the pool's jobs
/// alive while it exists.
pub struct Helper<J: Job> {
    shared: Arc<Shared<J>>,
}

impl<J: Job> Helper<J> {
    /// Renders jobs of the block being run until none are left to take,
    /// then returns. Between blocks it returns at once. It does not
    /// allocate, lock or wait.
    pub fn help(&self) {
        let block = self.shared.block.0.load(Ordering::Acquire);
        self.shared.take_jobs(block);
    }
}

impl<J: Job> Clone for Helper<J> {
    fn clone(&self) -> Self {
        Helper {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<J: Job> std::fmt::Debug for Helper<J> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Helper").finish_non_exhaustive()
    }
}

/// A worker's life: ask for a priority, then render each new block and
/// park in between.
fn work<J: Job>(shared: &Shared<J>, real_time: bool, report: mpsc::Sender<Priority>) {
    let promotion = if real_time {
        Promotion::current_thread()
    } else {
        Promotion::none()
    };
    let _ = report.send(promotion.priority());
    drop(report);
    let mut seen = 0;
    loop {
        let block = shared.block.0.load(Ordering::Acquire);
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }
        if block != seen {
            seen = block;
            shared.take_jobs(block);
        } else {
            thread::park();
            shared.wakeups.fetch_add(1, Ordering::Relaxed);
        }
    }
    drop(promotion);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Count(u64);

    impl Job for Count {
        fn render(&mut self) {
            self.0 += 1;
        }
    }

    #[test]
    fn every_job_asked_for_renders_once_per_block() {
        for workers in [0, 1, 3] {
            let mut pool = RenderPool::new(
                (0..10).map(|_| Count(0)).collect(),
                PoolConfig::with_workers(workers).wait_limit(Duration::from_secs(5)),
            );
            for _ in 0..20 {
                assert!(pool.run(6).is_complete());
            }
            for index in 0..10 {
                let expected = if index < 6 { 20 } else { 0 };
                assert_eq!(pool.job(index).unwrap().0, expected);
                assert_eq!(pool.was_rendered(index), index < 6);
            }
            assert_eq!(pool.rendered().count(), 6);
            assert_eq!(pool.stats().blocks, 20);
        }
    }

    #[test]
    fn a_count_beyond_the_jobs_renders_them_all() {
        let mut pool = RenderPool::new(vec![Count(0), Count(0)], PoolConfig::offline());
        let report = pool.run(usize::MAX);
        assert_eq!(
            report,
            BlockReport {
                rendered: 2,
                missed: 0
            }
        );
        let mut empty = RenderPool::<Count>::new(Vec::new(), PoolConfig::with_workers(1));
        assert_eq!(empty.run(3), BlockReport::default());
        assert!(empty.is_empty());
    }

    #[test]
    fn a_helper_between_blocks_renders_nothing() {
        let mut pool = RenderPool::new(vec![Count(0)], PoolConfig::offline());
        pool.run(1);
        pool.helper().help();
        assert_eq!(pool.job(0).unwrap().0, 1);
    }
}
