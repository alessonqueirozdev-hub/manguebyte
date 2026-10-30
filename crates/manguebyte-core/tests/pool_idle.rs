//! Proves that an idle pool's workers use next to no processor time.
//!
//! It is a test binary of its own, with one test, so that nothing else runs
//! in the process while it measures the process's processor time.

#![cfg(all(not(miri), any(windows, all(unix, target_endian = "little"))))]

use std::thread;
use std::time::Duration;

use manguebyte_core::pool::{Job, PoolConfig, RenderPool};

/// The processor time the whole process has used, user and kernel.
#[cfg(windows)]
fn process_time() -> Duration {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut u64,
            exit: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
    }

    let (mut creation, mut exit, mut kernel, mut user) = (0, 0, 0, 0);
    // SAFETY: the pseudo-handle of this process, and four `FILETIME`s (two
    // 32-bit halves, read here as one little-endian `u64`) that are ours to
    // write.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    assert_ne!(ok, 0);
    // In units of 100 ns.
    Duration::from_nanos((kernel + user) * 100)
}

/// The processor time the whole process has used, user and system.
#[cfg(unix)]
fn process_time() -> Duration {
    use std::ffi::c_int;

    unsafe extern "C" {
        fn getrusage(who: c_int, usage: *mut i64) -> c_int;
    }

    // `struct rusage` starts with two `timeval`s, user then system time:
    // 64-bit seconds, then microseconds, which macOS keeps in the low 32
    // bits of the second word. The rest of the buffer covers the other
    // fields.
    let mut usage = [0i64; 32];
    // SAFETY: `RUSAGE_SELF` (0), and a buffer larger than `struct rusage`.
    assert_eq!(unsafe { getrusage(0, usage.as_mut_ptr()) }, 0);
    let time = |seconds: i64, micros: i64| {
        Duration::from_secs(seconds as u64) + Duration::from_micros(micros as i32 as u64)
    };
    time(usage[0], usage[1]) + time(usage[2], usage[3])
}

struct Roots(Vec<f32>);

impl Job for Roots {
    fn render(&mut self) {
        for (index, sample) in self.0.iter_mut().enumerate() {
            *sample = (*sample + index as f32).sqrt();
        }
    }
}

#[test]
fn an_idle_pool_uses_almost_no_processor_time() {
    let workers = PoolConfig::all_cores().workers().max(3);
    let mut pool = RenderPool::new(
        (0..64).map(|_| Roots(vec![1.0; 128])).collect(),
        PoolConfig::with_workers(workers),
    );
    for _ in 0..200 {
        pool.run(64);
    }
    thread::sleep(Duration::from_millis(50));
    let wakeups = pool.stats().worker_wakeups;
    let before = process_time();
    let idle = Duration::from_secs(1);
    thread::sleep(idle);
    let used = process_time() - before;
    let woken = pool.stats().worker_wakeups - wakeups;
    println!("{workers} idle workers for {idle:?}: {used:?} of processor time, {woken} wake-ups");
    // One spinning worker would use the whole second. Windows counts
    // processor time in ticks of 15.6 ms.
    assert!(used < Duration::from_millis(50), "{used:?}");
    assert!(woken <= workers as u64, "{woken} wake-ups");
}
