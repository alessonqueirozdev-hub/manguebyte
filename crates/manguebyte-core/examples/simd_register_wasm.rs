//! Checks `manguebyte_core::simd` and `manguebyte_core::lookup` in
//! WebAssembly, with the checks the native tests make: every operation of a
//! register against its definition, and the fingerprint of the lookup tables.
//!
//! ```text
//! RUSTFLAGS="-C target-feature=+simd128" cargo build --release \
//!   -p manguebyte-core --example simd_register_wasm --target wasm32-unknown-unknown
//! node crates/manguebyte-core/examples/simd_register_wasm.mjs \
//!   target/wasm32-unknown-unknown/release/examples/simd_register_wasm.wasm
//! ```
//!
//! The module needs nothing from its host. Built natively, as `cargo test`
//! builds every example, it is a library nothing calls.

#[path = "../tests/support/simd_conformance.rs"]
mod conformance;
#[path = "../tests/support/lookup_golden.rs"]
mod golden;

use manguebyte_core::simd::Backend;

/// Whether the module runs the SIMD backend.
#[unsafe(no_mangle)]
pub extern "C" fn runs_simd128() -> u32 {
    u32::from(Backend::detect() == Backend::Simd128)
}

/// Runs the checks on every backend the module has. Gives 0 if all pass,
/// and otherwise a bit for each kind that failed: 1 and 2 for the
/// operations on `f32` and `f64`, 4 and 8 for the recursive loop, 16 for
/// the lookup tables.
#[unsafe(no_mangle)]
pub extern "C" fn failed_checks() -> u32 {
    let mut failed = 0;
    for backend in Backend::available() {
        let ran = conformance::every_operation::<f32>(backend, 2_000, false);
        failed |= u32::from(ran != Ok(backend));
        let ran = conformance::every_operation::<f64>(backend, 2_000, false);
        failed |= u32::from(ran != Ok(backend)) << 1;
        for frames in [1, 63, 48_000] {
            failed |= u32::from(conformance::resonators::<f32>(backend, frames).is_err()) << 2;
            failed |= u32::from(conformance::resonators::<f64>(backend, frames).is_err()) << 3;
        }
    }
    failed | u32::from(golden::fingerprint() != golden::FINGERPRINT) << 4
}
