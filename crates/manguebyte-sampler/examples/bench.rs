//! Times the sampler's voices and prints what one voice costs.
//!
//! Run it in release mode:
//!
//! ```text
//! cargo run -p manguebyte-sampler --example bench --release
//! ```
//!
//! Each case plays 32 looping voices of a stereo sample into a stereo
//! output at 48 kHz, in blocks of 128 frames, for two seconds of audio,
//! three times. The table gives the fastest run as nanoseconds per voice and
//! output frame, and how many such voices one core renders in real time.
//! Times depend on the machine and on what else it is doing; the numbers
//! are for comparing cases on one machine. `wasm_bench` times the same
//! cases in WebAssembly.

#[path = "common/cases.rs"]
mod cases;

use std::time::Instant;

use manguebyte_core::AudioBuffer;

fn main() {
    let seconds = 2;
    let blocks = seconds * cases::RATE as usize / cases::BLOCK;
    let voice_frames = (blocks * cases::BLOCK * cases::VOICES) as f64;
    println!(
        "{:<40} {:>14} {:>16}",
        "case", "ns/voice/frame", "real-time voices"
    );
    let mut checksum = 0.0;
    for case in cases::CASES {
        let mut best = f64::INFINITY;
        for _ in 0..3 {
            let mut sampler = cases::sampler(case);
            let mut buffer = AudioBuffer::<f32>::new(2, cases::BLOCK);
            let start = Instant::now();
            checksum += cases::render(&mut sampler, &mut buffer, blocks);
            best = best.min(start.elapsed().as_secs_f64());
            assert_eq!(sampler.active_voices(), cases::VOICES);
        }
        let ns = best * 1e9 / voice_frames;
        let voices = 1e9 / (ns * cases::RATE);
        println!("{:<40} {ns:>14.1} {voices:>16.0}", case.name);
    }
    println!("(checksum {checksum:.3})");
}
