//! FNV-1a, a small hash for naming large data in a scene's dump.
//!
//! It is not for security: it only has to change when the data changes,
//! and to be the same on every platform, which it is, since it works on
//! bytes in a fixed order.

/// A running 64-bit FNV-1a hash.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fnv(u64);

impl Fnv {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    pub(crate) fn new() -> Self {
        Fnv(Self::OFFSET)
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes(&value.to_le_bytes());
    }

    pub(crate) fn f32(&mut self, value: f32) {
        self.u32(value.to_bits());
    }

    pub(crate) fn finish(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_published_test_vectors() {
        // From the FNV reference test suite (Fowler, Noll, Vo).
        let mut empty = Fnv::new();
        empty.bytes(b"");
        assert_eq!(empty.finish(), 0xcbf2_9ce4_8422_2325);
        let mut a = Fnv::new();
        a.bytes(b"a");
        assert_eq!(a.finish(), 0xaf63_dc4c_8601_ec8c);
        let mut foobar = Fnv::new();
        foobar.bytes(b"foobar");
        assert_eq!(foobar.finish(), 0x8594_4171_f739_67e8);
    }
}
