//! The SIMD registers on every backend this machine has, against the
//! definition of each operation in plain Rust.

#[path = "support/simd_conformance.rs"]
mod conformance;

use manguebyte_core::NoDenormals;
use manguebyte_core::simd::{self, Aligned, Backend, Isa, Kernel, Register, SimdSample};

/// Miri runs the scalar code only, and slowly.
const BLOCKS: usize = if cfg!(miri) { 4 } else { 4_000 };

#[test]
fn every_operation_on_f32_is_the_definition_on_every_backend() {
    for backend in Backend::available() {
        let ran = conformance::every_operation::<f32>(backend, BLOCKS, false)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(ran, backend, "the kernel ran on the backend asked for");
    }
}

#[test]
fn every_operation_on_f64_is_the_definition_on_every_backend() {
    for backend in Backend::available() {
        let ran = conformance::every_operation::<f64>(backend, BLOCKS, false)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(ran, backend);
    }
}

/// With subnormals flushed, as on an audio thread, every backend still does
/// what plain arithmetic does in the same mode, but that `min`, `max` and
/// what is built on them may give a subnormal operand back as zero.
#[test]
fn the_backends_agree_with_subnormals_flushed() {
    let _flushed = NoDenormals::new();
    for backend in Backend::available() {
        conformance::every_operation::<f32>(backend, BLOCKS, true)
            .unwrap_or_else(|error| panic!("{error}"));
        conformance::every_operation::<f64>(backend, BLOCKS, true)
            .unwrap_or_else(|error| panic!("{error}"));
    }
}

#[test]
fn a_recursive_loop_gives_the_same_bits_on_every_backend() {
    for backend in Backend::available() {
        for frames in [0, 1, 7, 8, 9, 63, if cfg!(miri) { 65 } else { 48_000 }] {
            conformance::resonators::<f32>(backend, frames)
                .unwrap_or_else(|error| panic!("{error}"));
            conformance::resonators::<f64>(backend, frames)
                .unwrap_or_else(|error| panic!("{error}"));
        }
    }
}

struct Which;

impl Kernel for Which {
    type Output = Backend;

    #[inline(always)]
    fn run<I: Isa>(self, isa: I) -> Backend {
        isa.backend()
    }
}

#[test]
fn dispatch_runs_the_backend_it_says() {
    assert_eq!(simd::dispatch(Which), Backend::detect());
    for backend in Backend::ALL {
        let expected = if backend.is_available() {
            backend
        } else {
            Backend::Scalar
        };
        assert_eq!(backend.dispatch(Which), expected);
    }
}

#[test]
fn a_register_is_as_wide_on_every_backend() {
    struct Widths;
    impl Kernel for Widths {
        type Output = [usize; 4];

        #[inline(always)]
        fn run<I: Isa>(self, _: I) -> [usize; 4] {
            [
                <I::F32 as Register>::LANES,
                <I::F64 as Register>::LANES,
                size_of::<I::F32>(),
                size_of::<I::F64>(),
            ]
        }
    }
    assert_eq!(<f32 as SimdSample>::LANES, 8);
    assert_eq!(<f64 as SimdSample>::LANES, 4);
    for backend in Backend::available() {
        assert_eq!(backend.dispatch(Widths), [8, 4, 32, 32]);
    }
    assert_eq!(align_of::<Aligned<[f32; 8]>>(), 32);
}

#[test]
fn a_clamp_turns_nan_into_the_low_bound() {
    struct Clamp;
    impl Kernel for Clamp {
        type Output = [f32; 8];

        #[inline(always)]
        fn run<I: Isa>(self, isa: I) -> [f32; 8] {
            isa.splat(f32::NAN)
                .clamp(isa.splat(-1.0f32), isa.splat(1.0f32))
                .to_array()
        }
    }
    for backend in Backend::available() {
        assert_eq!(backend.dispatch(Clamp), [-1.0; 8]);
    }
}

/// A kernel that loads from, or stores to, a slice one element short.
struct Short {
    store: bool,
}

impl Kernel for Short {
    type Output = ();

    #[inline(always)]
    fn run<I: Isa>(self, isa: I) {
        let mut short = [0.0f32; 7];
        if self.store {
            isa.splat(1.0f32).store(&mut short);
        } else {
            let _ = isa.load(&short);
        }
    }
}

#[test]
#[should_panic(expected = "out of range")]
fn loading_a_short_slice_panics() {
    simd::dispatch(Short { store: false });
}

#[test]
#[should_panic(expected = "out of range")]
fn storing_to_a_short_slice_panics() {
    simd::dispatch(Short { store: true });
}
