/// A compact, stable number standing for a parameter.
///
/// Parameters are named by text identifiers, which are right for saved state
/// and for people but too heavy to carry in events on the audio thread. A
/// `ParamId` is a hash of that text: the same identifier always gives the
/// same number, in every run and on every machine, so it can be stored in
/// sessions and automation.
///
/// ```
/// use manguebyte_core::ParamId;
///
/// const CUTOFF: ParamId = ParamId::from_name("cutoff");
/// assert_eq!(CUTOFF, ParamId::from_name("cutoff"));
/// assert_ne!(CUTOFF, ParamId::from_name("resonance"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParamId(pub u32);

impl ParamId {
    /// The id of the parameter with this text identifier.
    ///
    /// Usable in constants, so a processor can match on ids without hashing
    /// anything at run time. The hash is 32-bit FNV-1a (Fowler, Noll and Vo).
    pub const fn from_name(name: &str) -> Self {
        let bytes = name.as_bytes();
        let mut hash: u32 = 0x811C_9DC5;
        let mut index = 0;
        while index < bytes.len() {
            hash ^= bytes[index] as u32;
            hash = hash.wrapping_mul(0x0100_0193);
            index += 1;
        }
        Self(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_published_fnv_1a_test_vectors() {
        assert_eq!(ParamId::from_name("").0, 0x811C_9DC5);
        assert_eq!(ParamId::from_name("a").0, 0xE40C_292C);
        assert_eq!(ParamId::from_name("foobar").0, 0xBF9C_F968);
    }

    #[test]
    fn is_usable_in_constants_and_matches() {
        const GAIN: ParamId = ParamId::from_name("gain");
        let label = match ParamId::from_name("gain") {
            GAIN => "gain",
            _ => "other",
        };
        assert_eq!(label, "gain");
    }
}
