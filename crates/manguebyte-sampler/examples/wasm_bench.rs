//! The cases of the `bench` example, in WebAssembly, for a JavaScript
//! engine to time.
//!
//! WebAssembly without a host has no clock, so the module only does the
//! work and the engine measures it:
//!
//! ```text
//! cargo build -p manguebyte-sampler --example wasm_bench --target wasm32-unknown-unknown --release
//! node crates/manguebyte-sampler/examples/wasm_bench.mjs \
//!   target/wasm32-unknown-unknown/release/examples/wasm_bench.wasm
//! ```
//!
//! It exports `bench_cases()`, the number of cases, `bench_voice_frames(blocks)`,
//! the voice frames `blocks` blocks render, and `bench_case(case, blocks)`,
//! which builds the case's sampler and renders `blocks` blocks of it. The
//! script times each case with and without rendering and keeps the
//! difference.

#[path = "common/cases.rs"]
mod cases;

use manguebyte_core::AudioBuffer;

/// How many cases there are.
// SAFETY: the exported name is unique in this example, which is a small
// library of its own; nothing else in the module defines a symbol of that
// name.
#[unsafe(no_mangle)]
pub extern "C" fn bench_cases() -> u32 {
    cases::CASES.len() as u32
}

/// How many voice frames `blocks` blocks of a case render.
// SAFETY: as for `bench_cases`, the name is unique in this module.
#[unsafe(no_mangle)]
pub extern "C" fn bench_voice_frames(blocks: u32) -> f64 {
    blocks as f64 * (cases::BLOCK * cases::VOICES) as f64
}

/// Builds case `case` and renders `blocks` blocks of it. Returns a sum of
/// the output, or NaN for a case that does not exist.
// SAFETY: as for `bench_cases`, the name is unique in this module.
#[unsafe(no_mangle)]
pub extern "C" fn bench_case(case: u32, blocks: u32) -> f64 {
    let Some(case) = cases::CASES.get(case as usize) else {
        return f64::NAN;
    };
    let mut sampler = cases::sampler(case);
    let mut buffer = AudioBuffer::<f32>::new(2, cases::BLOCK);
    cases::render(&mut sampler, &mut buffer, blocks as usize)
}
