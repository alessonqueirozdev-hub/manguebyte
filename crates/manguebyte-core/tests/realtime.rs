//! Proves that writing to a log never touches the heap.
//!
//! [`LogQueue`] promises that posting is safe on the audio thread. This test
//! replaces the global allocator with one that counts, and fails if posting
//! a message, formatted or not, kept or dropped, allocates, reallocates or
//! frees.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use manguebyte_core::log::{LogLevel, LogQueue, MESSAGE_BYTES, SLOTS};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
        // `try_with` because the allocator is still called while a thread's
        // locals are being torn down.
        let _ = WATCHING.try_with(|watching| {
            if watching.get() {
                let _ = HEAP_CALLS.try_with(|calls| calls.set(calls.get() + 1));
            }
        });
    }
}

// SAFETY: every method forwards to the system allocator unchanged; the only
// addition is bumping a thread-local counter, which does not allocate.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `work` and returns how many times it called the allocator.
fn heap_calls(work: impl FnOnce()) -> usize {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    HEAP_CALLS.with(Cell::get)
}

#[test]
fn posting_never_allocates() {
    let log = Box::new(LogQueue::new());
    let long = "x".repeat(MESSAGE_BYTES * 2);
    let calls = heap_calls(|| {
        for index in 0..SLOTS + 8 {
            // Kept until the slots run out, then counted as lost.
            log.post(LogLevel::Info, "plain text");
            log.post(LogLevel::Warning, &long);
            log.post_fmt(
                LogLevel::Error,
                format_args!("voice {index} at {:.3} dB, {}", -6.0206_f32, true),
            );
            log.post_fmt(LogLevel::Debug, format_args!("{long}{long}"));
        }
        // Filtered out.
        log.set_max_level(LogLevel::Error);
        log.post_fmt(LogLevel::Trace, format_args!("{:?}", [1.5_f64; 8]));
    });
    assert_eq!(calls, 0, "posting allocated");
    let drained = log.drain();
    assert_eq!(drained.records.len(), SLOTS);
    assert!(drained.lost > 0);
}

#[test]
fn pulling_audio_from_memory_never_allocates() {
    use manguebyte_core::AudioBuffer;
    use manguebyte_core::source::{AudioSource, BufferSource, PositionableSource};

    let mut buffer = AudioBuffer::<f32>::new(2, 1_000);
    buffer.channel_mut(0).fill(0.5);
    let mut memory = BufferSource::new(buffer).looping(true);
    memory.prepare(48_000.0, 512);
    let mut out = AudioBuffer::<f32>::new(3, 512);
    let calls = heap_calls(|| {
        for frames in [512, 1, 300, 511, 0, 512] {
            out.set_num_frames(frames);
            memory.pull(&mut out);
        }
        memory.set_loop(Some(100..200));
        memory.seek(150);
        memory.pull(&mut out);
        memory.set_loop(None);
        memory.seek(u64::MAX);
        memory.pull(&mut out);
    });
    assert_eq!(calls, 0, "pulling from memory used the heap");
}

#[test]
fn vector_operations_never_allocate() {
    use manguebyte_core::AudioBuffer;
    use manguebyte_core::vector::{self, Backend};

    let mut a = vec![0.25f32; 1_027];
    let b = vec![-0.5f32; 1_027];
    let mut wide = vec![0.125f64; 515];
    let other = vec![2.0f64; 515];
    let mut interleaved = vec![0.0f32; 2 * 1_027];
    let mut bytes = vec![0u8; 4 * 1_027];
    let mut buffer = AudioBuffer::<f32>::new(2, 1_027);
    let source = AudioBuffer::<f32>::new(2, 1_027);
    let mut frames = vec![0.0f32; 2 * 1_027];
    let calls = heap_calls(|| {
        // The first use asks the processor what it has.
        let _ = Backend::detect();
        for backend in Backend::ALL {
            backend.scale(&mut a, 0.5);
            backend.copy_scaled(&mut a, &b, 0.5);
            backend.add(&mut a, &b);
            backend.add_scaled(&mut a, &b, 0.5);
            backend.multiply(&mut a, &b);
            backend.scale_ramp(&mut a, 0.0, 0.001);
            backend.scale_ramp(&mut wide, 1.0, -0.001);
            backend.add_scaled(&mut wide, &other, 0.5);
            let _ = backend.peak(&a) + backend.dot(&a, &b) + backend.dot(&wide, &other) as f32;
            let _ = backend.sum_of_squares(&a) + backend.sum_of_squares(&wide);
            backend.complex_multiply_add(&mut a[..1_026], &b[..1_026], &b[..1_026]);
            backend.interleave_stereo(&mut interleaved, &a, &b);
            let (left, right) = interleaved.split_at_mut(1_027);
            backend.deinterleave_stereo(left, right, &frames);
            backend.f32_to_i16_le(&mut bytes[..2 * 1_027], &a);
            backend.i16_le_to_f32(&mut a, &bytes[..2 * 1_027]);
            backend.f32_to_i24_le(&mut bytes[..3 * 1_027], &a);
            backend.i24_le_to_f32(&mut a, &bytes[..3 * 1_027]);
            backend.i32_le_to_f32(&mut a, &bytes);
        }
        vector::add(&mut a, &b);
        buffer.apply_gain(0.5);
        buffer.apply_gain_ramp(0.0, 1.0);
        buffer.mix_from(&source, 0.25);
        let _ = buffer.peak() + buffer.rms(1);
        buffer.copy_to_interleaved(&mut frames);
        buffer.copy_from_interleaved(&frames);
    });
    assert_eq!(calls, 0, "a vector operation used the heap");
}

#[test]
fn running_a_pool_never_allocates() {
    use std::time::{Duration, Instant};

    use manguebyte_core::pool::{Job, PoolConfig, RenderPool};

    /// Fills its buffer for a while, so that the audio thread also waits
    /// for workers, which reads the clock.
    struct Busy {
        out: Vec<f32>,
        seed: f32,
    }

    impl Job for Busy {
        fn render(&mut self) {
            let start = Instant::now();
            let length = Duration::from_micros(if cfg!(miri) { 1 } else { 40 });
            while start.elapsed() < length {
                for sample in &mut self.out {
                    self.seed = (self.seed * 1.000_1 + 0.001).fract();
                    *sample = self.seed;
                }
            }
        }
    }

    let jobs = (0..24)
        .map(|index| Busy {
            out: vec![0.0; 128],
            seed: index as f32 / 24.0,
        })
        .collect();
    let mut pool = RenderPool::new(jobs, PoolConfig::with_workers(3));
    let helper = pool.helper();
    let mut mix = [0.0f32; 128];
    // A first block, so that nothing below is done for the first time.
    pool.run(24);
    let blocks = if cfg!(miri) { 2 } else { 50 };
    let calls = heap_calls(|| {
        for block in 0..blocks {
            // Every path: waiting for workers within the limit, giving up
            // on them at once, and offering the work to helpers.
            pool.set_wait_limit(if block % 5 == 4 {
                Duration::ZERO
            } else {
                Duration::from_secs(1)
            });
            let _ = if block % 2 == 0 {
                pool.run(24)
            } else {
                pool.run_with(block % 24, |_| false)
            };
            mix.fill(0.0);
            for (_, job) in pool.rendered() {
                for (sum, sample) in mix.iter_mut().zip(&job.out) {
                    *sum += sample;
                }
            }
            for index in 0..24 {
                if let Some(job) = pool.job_mut(index) {
                    job.seed *= 0.5;
                }
            }
            helper.help();
        }
        let _ = pool.stats();
    });
    assert_eq!(calls, 0, "running the pool used the heap");
}

#[test]
fn governing_never_allocates() {
    use std::time::Duration;

    use manguebyte_core::governor::{Allowance, Governor, GovernorConfig};

    let mut governor = Governor::new(GovernorConfig::default());
    governor.set_sample_rate(48_000.0);
    governor.set_quality_floor(4.0);
    let calls = heap_calls(|| {
        for block in 0..2_000u32 {
            let mut allowance = Allowance::new(governor.budget());
            governor.begin_block();
            for _voice in 0..64 {
                if !allowance.try_take(32.0) {
                    allowance.take_up_to(8.0);
                }
            }
            if block % 97 == 0 {
                governor.note_missed(1);
            }
            governor.end_block(128, allowance.spent(), allowance.limited());
            // Some blocks are timed by the caller, as around a pool.
            governor.record(
                Duration::from_micros(u64::from(block % 5) * 900),
                64,
                100.0,
                true,
            );
        }
        governor.set_offline(true);
        governor.begin_block();
        governor.end_block(128, 1.0, false);
        governor.set_offline(false);
        governor.set_max_budget(1_000.0);
        governor.reset();
        let _ = (governor.load(), governor.stats(), governor.unit_load());
    });
    assert_eq!(calls, 0, "the governor used the heap");
}
