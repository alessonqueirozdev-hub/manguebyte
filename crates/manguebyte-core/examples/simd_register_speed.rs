//! Times loops written over `manguebyte_core::simd` registers on every
//! backend this processor has, against the same loops in plain Rust, and the
//! lookup tables against the system's functions.
//!
//! ```text
//! cargo run --release -p manguebyte-core --example simd_register_speed
//! ```
//!
//! It prints nanoseconds per sample for blocks of 512 samples; each figure is
//! the best of several runs of at least 20 ms, so other work on the machine
//! shows less. Build it optimised: the debug figures mean nothing.

use std::hint::black_box;
use std::time::{Duration, Instant};

use manguebyte_core::lookup::{LookupTransform, SineTable};
use manguebyte_core::simd::{Backend, Isa, Kernel, Register, SimdSample};

const FRAMES: usize = 512;
const VOICES: usize = 8;

/// Nanoseconds per sample of `work`, which does `samples` of them: the best
/// of several timed runs.
fn time(samples: usize, mut work: impl FnMut()) -> f64 {
    work();
    let mut best = f64::INFINITY;
    for _ in 0..7 {
        let start = Instant::now();
        let mut calls = 0u64;
        while start.elapsed() < Duration::from_millis(20) {
            for _ in 0..16 {
                work();
            }
            calls += 16;
        }
        best = best.min(start.elapsed().as_secs_f64() / calls as f64);
    }
    best * 1e9 / samples as f64
}

/// A rational approximation of a soft clipper, `z (27 + z^2) / (27 + 9 z^2)`
/// with `z` the input under a gain, held between -3 and 3.
struct Shaper<'a> {
    input: &'a [f32],
    output: &'a mut [f32],
    gain: f32,
}

impl Kernel for Shaper<'_> {
    type Output = ();

    #[inline(always)]
    fn run<I: Isa>(self, isa: I) {
        let gain = isa.splat(self.gain);
        let (low, high) = (isa.splat(-3.0f32), isa.splat(3.0f32));
        let (a, b) = (isa.splat(27.0f32), isa.splat(9.0f32));
        let lanes = <f32 as SimdSample>::LANES;
        for (x, y) in self
            .input
            .chunks_exact(lanes)
            .zip(self.output.chunks_exact_mut(lanes))
        {
            let z = (isa.load(x) * gain).clamp(low, high);
            let square = z * z;
            (z * (a + square) / a.add_product(b, square)).store(y);
        }
    }
}

fn shaper_plain(input: &[f32], output: &mut [f32], gain: f32) {
    for (x, y) in input.iter().zip(output) {
        let z = (x * gain).clamp(-3.0, 3.0);
        let square = z * z;
        *y = z * (27.0 + square) / (27.0 + 9.0 * square);
    }
}

/// Eight state-variable resonators on one input, one output each, a frame of
/// the eight after another.
struct Bank<'a> {
    input: &'a [f32],
    output: &'a mut [f32],
    coefficients: [f32; VOICES],
}

impl Kernel for Bank<'_> {
    type Output = ();

    #[inline(always)]
    fn run<I: Isa>(self, isa: I) {
        let f = <I::F32 as Register>::from_array(isa, self.coefficients);
        let damping = isa.splat(0.01f32);
        let mut low = <I::F32 as Register>::zero(isa);
        let mut band = <I::F32 as Register>::zero(isa);
        for (x, y) in self.input.iter().zip(self.output.chunks_exact_mut(VOICES)) {
            low = low.add_product(f, band);
            let high = isa.splat(*x) - low - damping * band;
            band = band.add_product(f, high);
            band.store(y);
        }
    }
}

/// The bank a resonator at a time, as one would write it for one voice.
fn bank_plain(input: &[f32], output: &mut [f32], coefficients: &[f32; VOICES]) {
    for (voice, f) in coefficients.iter().enumerate() {
        let (mut low, mut band) = (0.0f32, 0.0f32);
        for (x, y) in input.iter().zip(output.chunks_exact_mut(VOICES)) {
            low += f * band;
            let high = x - low - 0.01 * band;
            band += f * high;
            y[voice] = band;
        }
    }
}

fn main() {
    let input: Vec<f32> = (0..FRAMES)
        .map(|i| ((i * 7_919 % 1_000) as f32 - 500.0) / 400.0)
        .collect();
    let mut output = vec![0.0f32; FRAMES * VOICES];
    let coefficients: [f32; VOICES] = core::array::from_fn(|i| 0.05 + 0.03 * i as f32);

    println!("ns per sample, blocks of {FRAMES}\n");
    println!(
        "{:<12}{:>14}{:>20}",
        "", "soft clipper", "8 resonators, each"
    );
    let shaper = time(FRAMES, || {
        shaper_plain(
            black_box(&input),
            black_box(&mut output[..FRAMES]),
            black_box(1.5),
        );
    });
    let bank = time(FRAMES * VOICES, || {
        bank_plain(
            black_box(&input),
            black_box(&mut output),
            black_box(&coefficients),
        );
    });
    println!("{:<12}{shaper:>14.3}{bank:>20.3}", "plain Rust");
    for backend in Backend::available() {
        let shaper = time(FRAMES, || {
            backend.dispatch(Shaper {
                input: black_box(&input),
                output: black_box(&mut output[..FRAMES]),
                gain: black_box(1.5),
            });
        });
        let bank = time(FRAMES * VOICES, || {
            backend.dispatch(Bank {
                input: black_box(&input),
                output: black_box(&mut output),
                coefficients: black_box(coefficients),
            });
        });
        println!("{:<12}{shaper:>14.3}{bank:>20.3}", backend.name());
    }

    let sine = SineTable::<f32>::new(2_048).expect("2048 points make a table");
    let tanh = LookupTransform::<f32>::tanh(4_096).expect("4096 points make a table");
    let mut block = vec![0.0f32; FRAMES];
    let mut read = |name: &str, function: &dyn Fn(&[f32], &mut [f32])| {
        let ns = time(FRAMES, || {
            function(black_box(&input), black_box(&mut block));
        });
        println!("{name:<24}{ns:>8.3}");
    };
    macro_rules! each {
        ($name:literal, |$x:ident| $value:expr) => {
            read($name, &|input, output| {
                for ($x, y) in input.iter().copied().zip(output) {
                    *y = $value;
                }
            });
        };
    }
    println!();
    each!("sin, system", |x| (x * core::f32::consts::TAU).sin());
    each!("sine table, linear", |x| sine.linear(x));
    each!("sine table, cubic", |x| sine.cubic(x));
    each!("tanh, system", |x| x.tanh());
    each!("tanh table, linear", |x| tanh.linear(x));
    each!("tanh table, cubic", |x| tanh.cubic(x));
}
