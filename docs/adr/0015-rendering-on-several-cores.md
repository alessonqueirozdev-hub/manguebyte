# ADR 0015: Rendering on several cores inside one audio callback

Status: accepted, 2026-10-30

## Context

One core is not enough for some instruments. Caecilia 2.0, a pipe organ
built on Manguebyte, plays hundreds of voices with thousands of partials and
sampled pipes besides; a full organ with every stop drawn does not fit in a
block's time on one core. Its work splits naturally into pieces that do not
depend on each other within a block: voices, divisions, chains of effects.
Hosts give a plugin one thread per callback, so a plugin that wants more
cores has to bring its own threads, or use the host's where the format
offers them (CLAP's `thread-pool` extension).

The rules of ADR 0002 still hold on the audio thread: nothing it runs may
allocate, lock or wait unboundedly. Rendering on other threads adds two
things that can break them: the audio thread has to wait for work done
elsewhere, and the threads that do it can be stopped by the system at any
moment (preempted by another process, their pages swapped out). And output
must not change: the framework's tests require bit-identical renders across
block patterns (`crates/manguebyte/tests/offline.rs`), and a render that
differs with the number of cores would break every one of them.

JUCE has `ThreadPool` (job objects run on a set of threads, waited for with
a timeout in milliseconds) and `TimeSliceThread` (one thread calling its
clients in turn). Its documentation presents both as general background
facilities; neither promises anything about use inside an audio callback,
the order results come back in, or what happens to a job still running when
the wait times out.

## Decision

**1. `manguebyte_core::pool`, in the core crate.** `RenderPool<J: Job>`,
`PoolConfig`, `Helper`, `Promotion` and `Priority`. It needs only `std`,
atomics and a few system calls, which the core crate already makes the way
`module::file` does, by declaring them itself (`avrt` on Windows,
`pthread_setschedparam` on Unix), so the crate still has no dependencies
and nothing new enters the workspace. A crate of its own would have been
a crate for three files.

**2. The pool owns its jobs.** A `Job` is `Send + 'static` and has one
method, `render(&mut self)`; it holds its input and its output buffer. The
pool is made with a `Vec` of jobs and a number of workers, off the audio
thread; the jobs are changed between blocks through `job_mut`, rendered by
`run(count)` (the first `count` of them), and read in index order through
`rendered()`. A job borrows nothing, because a job may still be running
after `run` has returned (decision 5): only memory the pool owns can safely
be written by a late worker.

**3. Fixed order, so bit-identical output.** A job is rendered whole by one
thread, and its result depends only on its own state; the caller mixes the
results in index order after `run`. The output is therefore exactly that of
rendering the same jobs one after another on one thread, whatever the
number of threads or which thread took which job. The mix is not spread
over the threads: summing 64 buffers of 128 frames costs microseconds.

**4. Taking jobs.** `run` publishes the block (a block number, the count
and a counter reset to zero), wakes as many parked workers as there are
jobs beyond the first, and then works like a worker: every thread takes the
next index with a `fetch_add` on the counter until it passes the count.
Each job has a flag, taken with an atomic swap by the one thread that may
touch it, and the number of the last block it was rendered for. When the
counter runs out, the audio thread sweeps the jobs not yet rendered and
renders itself any whose flag is free, including one whose index a worker
drew but has not yet begun. So the audio thread is never idle while a job
is waiting to be taken, and what it waits for is only jobs a worker is
rendering at that moment.

**5. A bounded wait, and what happens past it.** That wait is a spin with a
deadline, the pool's wait limit (one millisecond unless set; a plugin sets
it in `prepare` from the block's budget). If a worker still holds a job
when it passes, `run` closes the block and returns:

- the job's block is dropped from the mix: it is not in `rendered()`, the
  block's `BlockReport` counts it missed, and `PoolStats` counts the block
  as an overrun;
- the worker finishes the job when the system runs it again; the job's own
  state moves on by one block whose sound is lost;
- until then `job` and `job_mut` return `None` for it, and later blocks
  count it missed rather than wait for it again.

Closing a block is a store of its number, sequentially consistent, paired
with the flag: a thread that takes a flag then checks the block is still
open, and `job_mut` checks the flag after the block is closed, so either the
taker sees the block closed and leaves the job alone, or the audio thread
sees the flag held and does not lend the job out. No job is ever touched by
two threads, nothing half-rendered is mixed, and the audio thread never
waits longer than the limit.

The alternatives were worse. Waiting for the late worker until it finishes
is an unbounded wait, and on a machine with fewer free cores than workers
the audio thread would spin while the worker it waits for cannot run.
Rendering the late job a second time on the audio thread would need two
copies of every job's state and a way to throw one away, which a voice with
an oscillator, an envelope and a filter does not have; done on the same
state it is a data race. Mixing the late result into the next block would
move that voice by a block, audibly, and only sometimes.

**6. Waking and parking.** Between blocks the workers park
(`std::thread::park`), so an idle pool uses no processor time. `run` wakes
them with `Thread::unpark`, which is an atomic swap and, only for a parked
thread, one system call that does not block: `WakeByAddressSingle` on
Windows, a futex wake on Linux, Android and FreeBSD,
`dispatch_semaphore_signal` on macOS. The clock is read only while waiting
for a held job (`QueryPerformanceCounter`, the vDSO's `clock_gettime`,
`mach_absolute_time`: no system call). Nothing else on the audio thread
calls the system, allocates or locks.

**7. Priorities.** Each worker asks, once, as it starts, for real-time
scheduling the system grants without administrator rights: MMCSS's "Pro
Audio" task on Windows, `SCHED_FIFO` on Linux, Android, FreeBSD and macOS,
trying priorities from 70 down to 1. A refusal is reported, not an error;
`RenderPool::priorities` says what each worker got. `Promotion` does the
same for any thread and gives the scheduling back when dropped.

**8. Offline.** A pool of zero workers renders every job on the calling
thread, in index order, through the same calls; it never reads the clock.

**9. The host's threads.** `run_with(count, request)` offers the block to
other threads first: `request` gets the number of helpers worth running and
returns `true` if it ran that many `Helper::help` calls to completion, which
is what CLAP's `clap_host_thread_pool::request_exec` does with a plugin's
`clap_plugin_thread_pool::exec`. The pool's own workers are then not woken.
The CLAP wrapper does not offer this yet; it needs a way for a plugin to
hand the wrapper its helper and to be called from `exec`, which is a change
to the plugin API of its own ([TECH_DEBT](../TECH_DEBT.md) item 107).

## Measured

`examples/pool_speed.rs`, on the Intel Core i3-1215U this project is built
on (two performance cores with two threads each, four efficiency cores),
while other builds ran on it (about half its processor time in use): 64
voices of 32 partials, blocks of 128 frames at 48 kHz paced as a device
calls them, 3 000 blocks, every block's mix checked against one thread's.

| Threads | Mean | 99th percentile | Worst | Speed-up | Wake latency (median, 99th, worst) |
|---|---|---|---|---|---|
| 1 | 6.47 ms | 9.70 ms | 10.46 ms | 1.00 | |
| 2 | 3.55 ms | 5.56 ms | 6.47 ms | 1.83 | 20 µs, 74 µs, 2.25 ms |
| 4 | 1.64 ms | 1.76 ms | 3.70 ms | 3.94 | 24 µs, 37 µs, 46 µs |
| 8 | 1.65 ms | 1.79 ms | 3.27 ms | 3.93 | 24 µs, 39 µs, 99 µs |

Every mix was bit-identical to one thread's; no block missed a job. Eight
threads gain nothing over four on this machine, whose efficiency cores
were busy with the other builds. A worker parked between blocks begins
work about 20 to 30 µs after `run` starts, a little more with more workers
to wake; that is about 1 % of a block of 128 frames at 48 kHz, but would
be a quarter of a block of 16 frames at 96 kHz. These figures are from one
loaded laptop and say nothing about other machines.

## Consequences

- A plugin that renders on several cores keeps its voices (or divisions)
  in a `RenderPool`, sets their input between blocks and mixes
  `rendered()` in order. Its output does not depend on the number of
  cores, so its offline tests run with any.
- A missed job is audible as that one voice dropping out for a block, and
  is counted, so a plugin can show it or lower its polyphony.
- `job_mut` can return `None` after a missed block. A plugin that must
  change a job then (a note-off) keeps the change and applies it on a later
  block.
- Dropping a pool joins its workers, waiting for a late one to finish its
  job: a pool is dropped off the audio thread.
- Workers are started with the pool. Plugins with several instances start
  several pools; sharing one pool between instances, and joining macOS's
  audio workgroups, are left over (TECH_DEBT item 107).

## Action items

1. [x] `RenderPool`, `Helper`, `Promotion` in `manguebyte-core`
2. [x] Tests: bit identity over 1 to 8 threads and five block sizes, a
   stalled worker, no allocation, workers ended on drop, idle processor
   time
3. [x] A benchmark (`examples/pool_speed.rs`)
4. [ ] The CLAP wrapper's `thread-pool` extension through `run_with`
5. [ ] macOS audio workgroups and the time-constraint policy for workers
