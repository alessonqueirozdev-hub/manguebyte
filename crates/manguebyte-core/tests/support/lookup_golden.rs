//! A fingerprint of what the lookup tables read, to compare between systems:
//! every bit of the ready-made tables, and of a table of a function made of
//! the four operations, over a fixed set of inputs.
//!
//! Shared by `tests/lookup.rs`, which the CI runs on Linux, Windows and
//! macOS, on x86-64 and AArch64, and by the `simd_register_wasm` example,
//! which runs in WebAssembly. It uses the public API only.

use manguebyte_core::lookup::{LookupTransform, SineTable};

/// What [`fingerprint`] gives on every system.
pub const FINGERPRINT: u64 = 0xae86_f45d_fd70_78b0;

/// The 64-bit FNV-1a hash (Fowler, Noll and Vo) of a stream of words.
struct Hash(u64);

impl Hash {
    fn add(&mut self, word: u64) {
        for byte in word.to_le_bytes() {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
}

macro_rules! read_every_table {
    ($t:ty, $hash:ident) => {{
        let sine = SineTable::<$t>::new(2_048).unwrap();
        let tanh = LookupTransform::<$t>::tanh(4_096).unwrap();
        let exp = LookupTransform::<$t>::exp(-6.0, 3.0, 1_000).unwrap();
        let own = LookupTransform::<$t>::new(-2.0, 5.0, 301, |x| x / (1.0 + x * x)).unwrap();
        for k in -6_000..=6_000i32 {
            let x = (f64::from(k) * 0.002_73) as $t;
            for y in [
                sine.linear(x),
                sine.cubic(x),
                tanh.linear(x),
                tanh.cubic(x),
                exp.linear(x),
                exp.cubic(x),
                own.linear(x),
                own.cubic(x),
            ] {
                $hash.add(u64::from(y.to_bits()));
            }
        }
        for x in [<$t>::NAN, <$t>::INFINITY, <$t>::NEG_INFINITY, <$t>::MAX] {
            for y in [sine.linear(x), tanh.cubic(x), exp.linear(x)] {
                $hash.add(u64::from(y.to_bits()));
            }
        }
    }};
}

pub fn fingerprint() -> u64 {
    let mut hash = Hash(0xCBF2_9CE4_8422_2325);
    read_every_table!(f32, hash);
    read_every_table!(f64, hash);
    hash.0
}
