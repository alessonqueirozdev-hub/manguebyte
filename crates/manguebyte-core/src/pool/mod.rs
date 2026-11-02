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
//! use std::time::Duration;
//!
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
//! // A plugin sets the wait limit from its block's budget; an example can
//! // afford to wait, so that a slow or busy machine misses nothing.
//! let config = PoolConfig::with_workers(2).wait_limit(Duration::from_secs(10));
//! let mut pool = RenderPool::new(voices, config);
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
//! A worker that merely wakes late, after the block is closed, finds nothing
//! to take and changes nothing: only a job that a block counted missed, or
//! one a block abandoned by a panic, is ever refused.
//!
//! This is why a pool owns its jobs, and why a job borrows nothing: a job
//! may still be running after `run` has returned.
//!
//! # Offline
//!
//! A pool of zero workers ([`PoolConfig::offline`]) renders every job on
//! the calling thread, in index order, through the same calls.

mod priority;

use std::cell::{Cell, UnsafeCell};
use std::hint;
use std::marker::PhantomData;
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

/// The low bits of a claim word: its state. The bits above are the block the
/// state is about (see [`claim_word`]).
const STATE: u64 = 0b11;
/// No block wants the job and no thread has it: as made, or given back by a
/// job that panicked, or by the end of a block that did not render it.
const FREE: u64 = 0;
/// The block wants the job and no thread has claimed it yet.
const OPEN: u64 = 1;
/// One thread has claimed the job and is the only one that may touch it.
const RENDERING: u64 = 2;
/// The job was rendered for the block; no thread has it.
const RENDERED: u64 = 3;

/// A claim word: `state` for `block`. Blocks count from 1 and never repeat;
/// the 62 bits left for them last over 100 000 years at a block per
/// microsecond, so a word of an earlier block never comes back. Within a
/// block, a job whose thread panicked is opened again: a thread still holding
/// that word then claims the job, which the block wants anyway.
const fn claim_word(block: u64, state: u64) -> u64 {
    (block << 2) | state
}

struct Slot<J> {
    /// Who may touch `job`, and for which block. Its moves, and who makes
    /// each of them:
    ///
    /// - `FREE` or an earlier block's `RENDERED` to `OPEN`: the audio thread,
    ///   the only one that opens, with a store, since no other thread writes
    ///   a word in those states;
    /// - `OPEN` to `RENDERING`: any thread, with a compare-and-swap, and the
    ///   winner has the job. A thread that loses, or that read the word in
    ///   an earlier block, fails and has written nothing;
    /// - `OPEN` to `FREE`: the audio thread, closing a block, with a
    ///   compare-and-swap against those claims;
    /// - `RENDERING` to `RENDERED`, or to `FREE` if the job panics: the
    ///   thread that claimed it, with a store, since no other thread writes
    ///   a word in that state.
    claim: AtomicU64,
    job: UnsafeCell<J>,
}

struct Shared<J> {
    slots: Box<[CachePadded<Slot<J>>]>,
    /// The last block opened, counted from 1: what tells a worker there is
    /// something new to look at.
    block: CachePadded<AtomicU64>,
    /// The next job to hand out in the current block. It and `count` only
    /// spread the work over the threads and may be seen stale: which thread
    /// has which job is decided by the claim words alone.
    next: CachePadded<AtomicUsize>,
    /// How many jobs the current block renders.
    count: AtomicUsize,
    shutdown: AtomicBool,
    wakeups: AtomicU64,
}

// SAFETY: a job is touched only by the thread whose compare-and-swap moved
// its slot from `OPEN` to `RENDERING`, until that thread's Release store moves
// the slot on, and by the audio thread when no block is open and the slot is
// not `RENDERING` (`job`, `job_mut`, `rendered`). So jobs move between threads
// (which needs `J: Send`) but are never shared: the pool is not `Sync`, so
// only one thread at a time can call those three. The argument takes one
// location at a time, as every thread sees one atomic's writes in one order:
//
// - Exclusion. Two threads cannot both claim, since a compare-and-swap reads
//   the latest word and only one can find it `OPEN`. A word is `OPEN` only
//   while its block is open: `Closing` takes back any slot still `OPEN`, also
//   with a compare-and-swap, so a claim and the end of the block are ordered
//   whichever comes first. After that the audio thread's own reads cannot go
//   back before the end of `OPEN`, so `RENDERING` is what they find if a
//   thread has the job, and a thread that comes late fails, writing nothing.
// - Visibility. The audio thread's Release store of `OPEN` is what a claim
//   (Acquire) reads, so it carries what `job_mut` wrote. A claiming thread's
//   Release store as it leaves `RENDERING` is what the audio thread reads
//   (Acquire) before it lends the job or opens the slot again, so it carries
//   what the job's rendering wrote, to the audio thread and through its next
//   `OPEN` to the next thread that claims.
unsafe impl<J: Send> Sync for Shared<J> {}
// SAFETY: as above.
unsafe impl<J: Send> Send for Shared<J> {}

/// Moves a claimed slot on when the claiming thread is done with it, also if
/// the job panics.
struct Hold<'a> {
    claim: &'a AtomicU64,
    /// The word to leave: `FREE` until the job has rendered.
    then: u64,
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        self.claim.store(self.then, Ordering::Release);
    }
}

/// Closes a block, however `run_with` ends: once this has dropped, no thread
/// can begin a job of the block.
struct Closing<'a, J> {
    shared: &'a Shared<J>,
    count: usize,
}

impl<J> Drop for Closing<'_, J> {
    fn drop(&mut self) {
        // When `run_with` gets this far normally its sweep has already left
        // no slot `OPEN`, and this only reads. A job or a `request` that
        // panics leaves some, and a job must not be lent while a thread can
        // still claim it.
        for slot in &self.shared.slots[..self.count] {
            let claim = &slot.0.claim;
            let word = claim.load(Ordering::Relaxed);
            if word & STATE == OPEN {
                // Failing means another thread has just claimed the job, and
                // the word then says so.
                let _ = claim.compare_exchange(word, FREE, Ordering::Relaxed, Ordering::Relaxed);
            }
        }
    }
}

impl<J: Job> Shared<J> {
    /// Renders job `index` if a block wants it and this thread gets it first.
    /// Returns whether this call rendered it.
    fn try_render(&self, index: usize) -> bool {
        // Relaxed, as this only saves a failing compare-and-swap: seen stale
        // it leaves the job to another thread or to `run_with`'s sweep.
        let word = self.slots[index].0.claim.load(Ordering::Relaxed);
        word & STATE == OPEN && self.render_from(index, word)
    }

    /// Claims job `index` if its slot still holds `word`, which says `OPEN`,
    /// renders it and returns `true`. Otherwise nothing has changed, however
    /// long ago `word` was read: the block number in it never comes back.
    fn render_from(&self, index: usize, word: u64) -> bool {
        let slot = &self.slots[index].0;
        let block = word >> 2;
        let claimed = slot.claim.compare_exchange(
            word,
            claim_word(block, RENDERING),
            Ordering::Acquire,
            Ordering::Relaxed,
        );
        if claimed.is_err() {
            return false;
        }
        let mut hold = Hold {
            claim: &slot.claim,
            then: FREE,
        };
        // SAFETY: this thread's compare-and-swap moved the slot from `OPEN`
        // to `RENDERING`, so it alone may touch the job until `hold` drops,
        // and reading `OPEN` with Acquire made all earlier writes to the job
        // visible (see `Shared`'s `Sync`).
        unsafe { (*slot.job.get()).render() };
        hold.then = claim_word(block, RENDERED);
        true
    }

    /// Takes jobs of the block being run until none are left to take.
    fn take_jobs(&self) {
        let count = self.count.load(Ordering::Relaxed);
        loop {
            let index = self.next.0.fetch_add(1, Ordering::AcqRel);
            if index >= count {
                break;
            }
            self.try_render(index);
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
    /// `job` and `rendered` lend jobs by `&self`, and a job need not be
    /// `Sync`, so the pool is not.
    not_sync: PhantomData<Cell<()>>,
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
                    claim: AtomicU64::new(FREE),
                    job: UnsafeCell::new(job),
                })
            })
            .collect();
        let shared = Arc::new(Shared {
            slots,
            block: CachePadded(AtomicU64::new(0)),
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
            not_sync: PhantomData,
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
    /// renders it, which happens only to a job a block has counted missed
    /// (or, after a panic in `run`, a job of the block it abandoned).
    ///
    /// The pool is not `Sync`, since a job need not be:
    ///
    /// ```compile_fail
    /// use manguebyte_core::pool::{Job, RenderPool};
    /// struct Voice(std::cell::Cell<f32>);
    /// impl Job for Voice {
    ///     fn render(&mut self) {}
    /// }
    /// fn shared_between_threads<T: Sync>() {}
    /// shared_between_threads::<RenderPool<Voice>>();
    /// ```
    pub fn job(&self, index: usize) -> Option<&J> {
        let slot = &self.shared.slots.get(index)?.0;
        if slot.claim.load(Ordering::Acquire) & STATE == RENDERING {
            return None;
        }
        // SAFETY: no block is open (`run_with` takes `&mut self` and closes
        // its block however it ends), so no slot is `OPEN` and no thread can
        // claim this one, and it is not `RENDERING`, so no thread has it.
        // The Acquire load saw the last rendering's Release store, so what
        // it wrote is visible here (see `Shared`'s `Sync`).
        Some(unsafe { &*slot.job.get() })
    }

    /// Job `index` to change between blocks, or `None` as for
    /// [`job`](Self::job). The change reaches the job's next rendering,
    /// whichever thread does it.
    ///
    /// A job refused here can render again before the program reaches it
    /// again: if its late worker lets it go before the next block, that
    /// block renders it with what it held, and reports it as rendered. So
    /// a program that keeps a change for a refused job should not mix that
    /// job's output until the change has gone in.
    pub fn job_mut(&mut self, index: usize) -> Option<&mut J> {
        let slot = &self.shared.slots.get(index)?.0;
        if slot.claim.load(Ordering::Acquire) & STATE == RENDERING {
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
                // SAFETY: `run_with` read this slot's `RENDERED` with Acquire
                // after closing the block, so the rendering's writes are
                // visible, and with no block open no thread can claim the
                // slot again (see `Shared`'s `Sync`).
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
        // If a panic ends this call, no job counts as rendered: any of them
        // may be claimed below and still be rendering when `rendered` runs.
        self.done.fill(false);
        let closing = Closing { shared, count };

        // Open the block in its slots, before anything tells a thread to
        // look. A slot still `RENDERING` belongs to a late worker and is left
        // alone. Any other word here is one only this thread can move on, so
        // the store cannot overwrite a claim. Acquire: if a late worker has
        // just finished, this is the first this thread hears of it, and the
        // Release store of `OPEN` passes its writes on to the next claim.
        for slot in &shared.slots[..count] {
            let claim = &slot.0.claim;
            if claim.load(Ordering::Acquire) & STATE != RENDERING {
                claim.store(claim_word(block, OPEN), Ordering::Release);
            }
        }
        shared.count.store(count, Ordering::Relaxed);
        shared.next.0.store(0, Ordering::Release);
        shared.block.0.store(block, Ordering::Release);

        let helpers = count.saturating_sub(1);
        if helpers > 0 && !request(helpers) {
            for worker in self.workers.iter().take(helpers) {
                worker.thread().unpark();
            }
        }

        shared.take_jobs();

        // Every job has been handed out. Render any no thread has begun,
        // and wait, up to the limit, for those another thread renders. One
        // whose thread has let go of it meanwhile (a late worker done with an
        // earlier block, a job that panicked) is free, so it is opened now
        // and is due like the others. One pass over a slot always leaves it
        // past `OPEN`, so past this loop no slot is left to take back.
        let rendered = claim_word(block, RENDERED);
        let mut deadline = None;
        let mut first = 0;
        loop {
            let mut pending = None;
            for index in first..count {
                let claim = &shared.slots[index].0.claim;
                let word = claim.load(Ordering::Acquire);
                if word == rendered {
                    continue;
                }
                if matches!(word & STATE, FREE | RENDERED) {
                    claim.store(claim_word(block, OPEN), Ordering::Release);
                }
                if !shared.try_render(index) && pending.is_none() {
                    pending = Some(index);
                }
            }
            let Some(index) = pending else { break };
            first = index;
            // A limit too long to add to the clock is no limit.
            let deadline =
                *deadline.get_or_insert_with(|| Instant::now().checked_add(self.wait_limit));
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                break;
            }
            hint::spin_loop();
        }

        // Close the block: from here no thread starts a job of it. Only a
        // word naming this block counts: a late worker that has just finished
        // an earlier block leaves `RENDERED` too, and that sound is lost.
        drop(closing);
        let mut report = BlockReport::default();
        for (index, done) in self.done.iter_mut().enumerate() {
            *done =
                index < count && shared.slots[index].0.claim.load(Ordering::Acquire) == rendered;
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
        self.shared.take_jobs();
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
            shared.take_jobs();
        } else {
            thread::park();
            shared.wakeups.fetch_add(1, Ordering::Relaxed);
        }
    }
    drop(promotion);
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

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

    fn counts(count: usize) -> RenderPool<Count> {
        RenderPool::new(
            (0..count).map(|_| Count(0)).collect(),
            PoolConfig::offline().wait_limit(Duration::ZERO),
        )
    }

    fn word_of<J>(shared: &Shared<J>, index: usize) -> u64 {
        shared.slots[index].0.claim.load(Ordering::SeqCst)
    }

    /// The failure this design removes, with no thread and no timing: a
    /// thread that read a slot in an open block, stopped, and goes on after
    /// the block closed. `job_mut` must lend the job at each point of its
    /// path, because no point of it writes anything.
    #[test]
    fn a_thread_that_comes_late_changes_nothing() {
        let mut pool = counts(2);
        let shared = Arc::clone(&pool.shared);
        let mut seen = 0;
        pool.run_with(2, |_| {
            seen = word_of(&shared, 1);
            false
        });
        assert_eq!(seen, claim_word(1, OPEN));
        let closed = word_of(&shared, 1);
        assert_eq!(closed, claim_word(1, RENDERED));

        // Before it looks at the slot again.
        assert!(pool.job_mut(1).is_some());
        // Looking, finding nothing to claim.
        assert!(!shared.try_render(1));
        assert_eq!(word_of(&shared, 1), closed);
        assert!(pool.job_mut(1).is_some());
        // Stopped before its compare-and-swap, which now fails.
        assert!(!shared.render_from(1, seen));
        assert_eq!(word_of(&shared, 1), closed);
        assert!(pool.job_mut(1).is_some());
        // And all the way round, as a worker does.
        shared.take_jobs();
        assert_eq!(word_of(&shared, 1), closed);
        assert_eq!(pool.job_mut(1).unwrap().0, 1);
    }

    #[test]
    fn a_word_read_in_one_block_claims_nothing_in_the_next() {
        let mut pool = counts(2);
        let shared = Arc::clone(&pool.shared);
        let mut seen = 0;
        pool.run_with(2, |_| {
            seen = word_of(&shared, 1);
            false
        });
        let report = pool.run_with(2, |_| {
            // The slot is open again, for another block.
            assert_eq!(word_of(&shared, 1), claim_word(2, OPEN));
            assert!(!shared.render_from(1, seen));
            assert_eq!(word_of(&shared, 1), claim_word(2, OPEN));
            false
        });
        assert!(report.is_complete());
        assert_eq!(pool.job(1).unwrap().0, 2);
    }

    /// Claims job `index` as a worker does, and keeps it: the job is then
    /// held by a thread that does not run, until the test sets the word.
    fn hold<J>(shared: &Shared<J>, index: usize) {
        let open = word_of(shared, index);
        assert_eq!(open & STATE, OPEN);
        let held = claim_word(open >> 2, RENDERING);
        let claim = &shared.slots[index].0.claim;
        let claimed = claim.compare_exchange(open, held, Ordering::Acquire, Ordering::Relaxed);
        assert!(claimed.is_ok());
    }

    /// What the thread `hold` stands for does when it finishes, late.
    fn let_go<J>(shared: &Shared<J>, index: usize, block: u64) {
        let word = claim_word(block, RENDERED);
        shared.slots[index].0.claim.store(word, Ordering::Release);
    }

    #[test]
    fn a_held_job_is_missed_and_not_lent_until_it_is_let_go() {
        let mut pool = counts(2);
        let shared = Arc::clone(&pool.shared);
        let report = pool.run_with(2, |_| {
            hold(&shared, 1);
            false
        });
        assert_eq!(
            report,
            BlockReport {
                rendered: 1,
                missed: 1
            }
        );
        assert!(pool.job(1).is_none());
        assert!(pool.job_mut(1).is_none());
        assert!(pool.job(0).is_some());

        // The next block leaves the slot to its thread.
        let held = claim_word(1, RENDERING);
        let report = pool.run_with(2, |_| {
            assert_eq!(word_of(&shared, 1), held);
            false
        });
        assert_eq!(report.missed, 1);
        assert_eq!(word_of(&shared, 1), held);
        assert!(pool.job(1).is_none());

        // Let go after block 2: what it rendered was for block 1, whose
        // sound is lost, so it does not join block 2.
        let_go(&shared, 1, 1);
        assert!(pool.job(1).is_some());
        assert!(pool.job_mut(1).is_some());
        assert!(!pool.was_rendered(1));
        assert_eq!(pool.rendered().count(), 1);
        let stats = pool.stats();
        assert_eq!((stats.blocks, stats.overruns, stats.missed_jobs), (2, 2, 2));

        // And the job is due again.
        assert!(pool.run(2).is_complete());
        assert!(pool.was_rendered(1));
        assert_eq!(pool.job(1).unwrap().0, 1);
        assert_eq!(pool.job(0).unwrap().0, 3);
    }

    #[test]
    fn a_wait_limit_too_long_for_the_clock_waits_without_a_panic() {
        let mut pool = counts(2);
        let shared = Arc::clone(&pool.shared);
        pool.run_with(2, |_| {
            hold(&shared, 1);
            false
        });
        pool.set_wait_limit(Duration::MAX);
        // The next block finds the job held, and waits until it is let go.
        let late = Arc::clone(&shared);
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let_go(&late, 1, 1);
        });
        let report = pool.run(2);
        worker.join().unwrap();
        assert!(report.is_complete());
        assert_eq!(pool.job(1).unwrap().0, 1);
    }

    #[test]
    fn a_job_let_go_during_a_block_renders_in_that_block() {
        let mut pool = counts(2);
        let shared = Arc::clone(&pool.shared);
        pool.run_with(2, |_| {
            hold(&shared, 1);
            false
        });
        let report = pool.run_with(2, |_| {
            let_go(&shared, 1, 1);
            false
        });
        assert!(report.is_complete());
        assert_eq!(word_of(&shared, 1), claim_word(2, RENDERED));
        assert_eq!(pool.job(1).unwrap().0, 1);
    }

    /// Runs what it holds, once, as it renders.
    struct Hook(Option<Box<dyn FnOnce() + Send>>);

    impl Job for Hook {
        fn render(&mut self) {
            if let Some(hook) = self.0.take() {
                hook();
            }
        }
    }

    /// A late worker that finishes between the sweep's last look at its slot
    /// and the end of the block leaves `RENDERED`, which must not pass for
    /// this block's.
    #[test]
    fn a_late_worker_finishing_as_the_block_closes_is_not_counted_in_it() {
        let mut pool = RenderPool::new(
            vec![Hook(None), Hook(None), Hook(None)],
            PoolConfig::offline().wait_limit(Duration::ZERO),
        );
        let shared = Arc::clone(&pool.shared);
        pool.run_with(3, |_| {
            let held = claim_word(1, RENDERING);
            shared.slots[1].0.claim.store(held, Ordering::SeqCst);
            false
        });
        assert!(!pool.was_rendered(1));

        // The sweep goes through the jobs in order, so a hook on the job
        // after the held one acts just behind the sweep's back.
        let behind = Arc::clone(&shared);
        pool.job_mut(2).unwrap().0 = Some(Box::new(move || {
            let word = claim_word(1, RENDERED);
            behind.slots[1].0.claim.store(word, Ordering::Release);
        }));
        let report = pool.run_with(3, |_| {
            // All handed out, as if workers had drawn them, so that the
            // audio thread renders them in the sweep.
            shared.next.0.store(usize::MAX / 2, Ordering::SeqCst);
            false
        });
        assert_eq!(word_of(&shared, 1), claim_word(1, RENDERED));
        assert_eq!(
            report,
            BlockReport {
                rendered: 2,
                missed: 1
            }
        );
        assert!(!pool.was_rendered(1));
        assert!(pool.job(1).is_some());
    }

    /// Panics when armed.
    struct Bomb {
        armed: bool,
    }

    impl Job for Bomb {
        fn render(&mut self) {
            assert!(!self.armed, "a job panicked on purpose");
        }
    }

    /// A pool that has rendered one clean block, and so has jobs counted as
    /// rendered for a panic to take back, with job 1 armed for the next.
    fn bombs() -> RenderPool<Bomb> {
        let jobs = (0..4).map(|_| Bomb { armed: false }).collect();
        let mut pool = RenderPool::new(jobs, PoolConfig::offline());
        assert!(pool.run(4).is_complete());
        assert_eq!(pool.rendered().count(), 4);
        pool.job_mut(1).unwrap().armed = true;
        pool
    }

    /// No slot can be claimed, every job can be lent, and the pool goes on.
    fn assert_closed_after_a_panic(pool: &mut RenderPool<Bomb>) {
        let shared = Arc::clone(&pool.shared);
        assert_eq!(pool.rendered().count(), 0);
        for index in 0..pool.len() {
            assert_ne!(word_of(&shared, index) & STATE, OPEN);
            pool.job_mut(index).unwrap().armed = false;
            assert!(!shared.try_render(index));
        }
        assert!(pool.run(4).is_complete());
        assert_eq!(pool.rendered().count(), 4);
    }

    #[test]
    fn a_job_that_panics_on_the_audio_thread_still_closes_the_block() {
        let mut pool = bombs();
        assert!(catch_unwind(AssertUnwindSafe(|| pool.run(4))).is_err());
        assert_closed_after_a_panic(&mut pool);
    }

    #[test]
    fn a_request_that_panics_still_closes_the_block() {
        let mut pool = bombs();
        let result = catch_unwind(AssertUnwindSafe(|| {
            pool.run_with(4, |_| panic!("a request panicked on purpose"))
        }));
        assert!(result.is_err());
        assert_closed_after_a_panic(&mut pool);
    }

    /// A thread that claimed a job of the abandoned block may still render
    /// it, so the job is neither lent nor counted as rendered until it is
    /// done, and a slot left `OPEN` is taken back.
    #[test]
    fn a_job_a_thread_holds_is_not_lent_after_a_panic() {
        let mut pool = bombs();
        pool.job_mut(1).unwrap().armed = false;
        let shared = Arc::clone(&pool.shared);
        let block = pool.block + 1;
        let result = catch_unwind(AssertUnwindSafe(|| {
            pool.run_with(4, |_| {
                hold(&shared, 1);
                assert_eq!(word_of(&shared, 2) & STATE, OPEN);
                panic!("a request panicked on purpose")
            })
        }));
        assert!(result.is_err());
        assert!(pool.job(1).is_none());
        assert!(pool.job_mut(1).is_none());
        assert_eq!(pool.rendered().count(), 0);
        assert_ne!(word_of(&shared, 2) & STATE, OPEN);
        assert!(pool.job(2).is_some());

        let_go(&shared, 1, block);
        assert!(pool.job(1).is_some());
        assert!(pool.run(4).is_complete());
        assert_eq!(pool.rendered().count(), 4);
    }
}
