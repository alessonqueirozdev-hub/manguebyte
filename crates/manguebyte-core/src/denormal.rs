//! Keeping subnormal numbers out of the audio path.

use core::marker::PhantomData;

/// While one of these is alive, the processor treats subnormal numbers as
/// zero on the current thread.
///
/// # Why
///
/// A sound that fades out does not reach zero. A filter's memory, a reverb
/// tail or an envelope follower keeps halving until its numbers are too
/// small for the normal floating-point format, and from there on they are
/// *subnormal* (also called denormal): still not zero, and on most
/// processors tens of times slower to compute with. The result is a plugin
/// whose processor load jumps when it should be idle, in the silence after
/// a note, which is enough to make a host drop audio.
///
/// Every processor can be told to round such numbers to zero instead. That
/// costs nothing and loses nothing that can be heard: the largest
/// subnormal `f32` is more than 750 dB below full scale.
///
/// # Who needs to use this
///
/// Usually nobody. The plugin wrappers, the standalone engine and the test
/// harness all hold one of these around every call to `Plugin::process`,
/// and the device streams hold one around every call to their callback, so
/// audio code is protected without asking. Create one yourself only on an
/// audio thread that none of those started, for example when rendering to
/// a file in a loop of your own.
///
/// ```
/// use manguebyte_core::NoDenormals;
///
/// fn render_to_file(output: &mut [f32]) {
///     let _quiet = NoDenormals::new();
///     // ... fill `output` ...
/// #   output.fill(0.0);
/// }
/// # render_to_file(&mut [0.0; 4]);
/// ```
///
/// The setting is put back when the value is dropped, so the host's own
/// code runs as it expects. Real-time safe.
///
/// # What it does not cover
///
/// It works on `x86_64` and `aarch64`. Elsewhere it does nothing, and
/// [`SUPPORTED`](Self::SUPPORTED) is `false`.
///
/// Rust does not say what floating-point code does while this setting is
/// changed: the compiler assumes the default. What that assumption is used
/// for is working out arithmetic at compile time, so the visible effect is
/// that an expression the compiler evaluated itself may give a subnormal
/// result where the processor would have given zero. No sound depends on
/// the difference. This is the same position every audio program is in,
/// in any language.
#[derive(Debug)]
pub struct NoDenormals {
    /// The control bits as they were found.
    saved: u64,
    /// The setting belongs to the thread it was made on.
    not_send: PhantomData<*mut ()>,
}

impl NoDenormals {
    /// Whether this build can change the setting on this kind of processor.
    pub const SUPPORTED: bool = control::SUPPORTED;

    /// Makes subnormal numbers count as zero on this thread until the
    /// returned value is dropped.
    #[must_use = "the setting is put back as soon as this value is dropped"]
    pub fn new() -> Self {
        let saved = control::read();
        control::write(saved | control::FLUSH);
        Self {
            saved,
            not_send: PhantomData,
        }
    }
}

impl Default for NoDenormals {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NoDenormals {
    fn drop(&mut self) {
        // Only the flush bits go back to what they were. The rest of the
        // register holds flags that arithmetic has set since.
        let now = control::read();
        control::write((now & !control::FLUSH) | (self.saved & control::FLUSH));
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
mod control {
    use core::arch::asm;

    pub const SUPPORTED: bool = true;

    /// In the MXCSR register: flush-to-zero (bit 15), for results, and
    /// denormals-are-zero (bit 6), for operands.
    pub const FLUSH: u64 = (1 << 15) | (1 << 6);

    pub fn read() -> u64 {
        let mut word = 0u32;
        // SAFETY: `stmxcsr` writes the 32-bit control register to the
        // address it is given, which is a local `u32`, and does nothing
        // else.
        unsafe {
            asm!(
                "stmxcsr [{}]",
                in(reg) &raw mut word,
                options(nostack, preserves_flags),
            );
        }
        u64::from(word)
    }

    pub fn write(word: u64) {
        let word = word as u32;
        // SAFETY: `ldmxcsr` loads the control register from a local `u32`.
        // The value is what `read` returned with only the two flush bits
        // changed, so no exception is unmasked and the rounding mode stays
        // as it was. See the type's documentation for what the compiler
        // assumes about this register.
        unsafe {
            asm!(
                "ldmxcsr [{}]",
                in(reg) &raw const word,
                options(nostack, preserves_flags, readonly),
            );
        }
    }
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
mod control {
    use core::arch::asm;

    pub const SUPPORTED: bool = true;

    /// In the FPCR register: flush-to-zero (bit 24).
    pub const FLUSH: u64 = 1 << 24;

    pub fn read() -> u64 {
        let word: u64;
        // SAFETY: reads the floating-point control register, which user
        // code is allowed to do, into a register.
        unsafe {
            asm!("mrs {}, fpcr", out(reg) word, options(nomem, nostack, preserves_flags));
        }
        word
    }

    pub fn write(word: u64) {
        // SAFETY: writes the floating-point control register, which user
        // code is allowed to do. The value is what `read` returned with
        // only the flush bit changed, so no trap is enabled and the
        // rounding mode stays as it was. See the type's documentation for
        // what the compiler assumes about this register.
        unsafe {
            asm!("msr fpcr, {}", in(reg) word, options(nomem, nostack, preserves_flags));
        }
    }
}

// Other processors, and Miri, which cannot run the instructions above.
#[cfg(any(miri, not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
mod control {
    pub const SUPPORTED: bool = false;
    pub const FLUSH: u64 = 0;

    pub fn read() -> u64 {
        0
    }

    pub fn write(_word: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;

    /// Half of the smallest normal number, worked out by the processor and
    /// not by the compiler.
    fn half_of_the_smallest() -> f32 {
        black_box(black_box(f32::MIN_POSITIVE) * black_box(0.5))
    }

    #[test]
    fn subnormal_results_are_zero_only_while_the_guard_lives() {
        assert!(half_of_the_smallest() > 0.0, "the default is to keep them");
        {
            let _quiet = NoDenormals::new();
            if NoDenormals::SUPPORTED {
                assert_eq!(half_of_the_smallest(), 0.0);
            }
            // Ordinary numbers are untouched.
            assert_eq!(black_box(black_box(0.25f32) * black_box(0.5)), 0.125);
        }
        assert!(half_of_the_smallest() > 0.0, "the setting was not put back");
    }

    #[test]
    fn a_subnormal_operand_counts_as_zero() {
        let scaled = || black_box(black_box(f32::from_bits(1)) * black_box(1.0e30f32));
        assert!(scaled() > 0.0);
        let _quiet = NoDenormals::new();
        // Guaranteed by the x86 setting; other processors may or may not.
        if cfg!(all(target_arch = "x86_64", not(miri))) {
            assert_eq!(scaled(), 0.0);
        }
    }

    #[test]
    fn guards_nest_and_the_outer_one_stays_in_force() {
        let outer = NoDenormals::new();
        drop(NoDenormals::new());
        if NoDenormals::SUPPORTED {
            assert_eq!(half_of_the_smallest(), 0.0);
        }
        drop(outer);
        assert!(half_of_the_smallest() > 0.0);
    }
}
