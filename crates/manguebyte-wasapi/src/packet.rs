//! Turning what a capture device hands over into the slices `capture` is
//! given. It runs on the stream's thread and allocates only in
//! [`Reader::new`].

// Only Windows' module calls these outside the tests.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::{Captured, Packet, SampleKind};

/// A period of silence in the stream's format, handed over for the packets
/// whose data is not to be read.
enum Silence {
    F32(Vec<f32>),
    I32(Vec<i32>),
    I16(Vec<i16>),
}

/// Reads a capture stream's packets.
pub(crate) struct Reader {
    silence: Silence,
}

/// `len` values of `T` at `data`, or `None` if `data` cannot hold them.
///
/// # Safety
///
/// `data` must be null or point to at least `len` values' worth of readable,
/// initialised bytes that nothing changes for `'a`.
unsafe fn borrowed<'a, T>(data: *const u8, len: usize) -> Option<&'a [T]> {
    if data.is_null() || data.align_offset(std::mem::align_of::<T>()) != 0 {
        return None;
    }
    // SAFETY: the caller's promise, the pointer is non-null and aligned
    // for `T`, and every bit pattern is a value of the sample types this
    // is used with.
    Some(unsafe { std::slice::from_raw_parts(data.cast::<T>(), len) })
}

impl Reader {
    /// A reader for packets of `kind`, ready to stand in silence for
    /// `period` values (frames times channels) at a time.
    pub(crate) fn new(kind: SampleKind, period: usize) -> Self {
        let period = period.max(1);
        Self {
            silence: match kind {
                SampleKind::F32 => Silence::F32(vec![0.0; period]),
                SampleKind::I32 { .. } => Silence::I32(vec![0; period]),
                SampleKind::I16 => Silence::I16(vec![0; period]),
            },
        }
    }

    /// Hands `sink` a packet of `len` values. A packet the driver marked
    /// `silent` is handed over as zeros without being read. So is one whose
    /// data is null or misaligned, which then also counts as a
    /// `discontinuity`; silence longer than a period is handed over a
    /// period at a time.
    ///
    /// Real-time safe if `sink` is.
    ///
    /// # Safety
    ///
    /// Unless `silent`, `data` must be null or point to at least `len`
    /// samples of the reader's kind that are readable, initialised and
    /// unchanged for the whole call.
    pub(crate) unsafe fn read(
        &self,
        data: *const u8,
        len: usize,
        silent: bool,
        discontinuity: bool,
        sink: &mut dyn FnMut(Packet<'_>),
    ) {
        if len == 0 {
            return;
        }
        let data = if silent { std::ptr::null() } else { data };
        // SAFETY: the caller's promise, for the slice borrowed only while
        // `sink` runs.
        let captured = unsafe {
            match &self.silence {
                Silence::F32(_) => borrowed(data, len).map(Captured::F32),
                Silence::I32(_) => borrowed(data, len).map(Captured::I32),
                Silence::I16(_) => borrowed(data, len).map(Captured::I16),
            }
        };
        if let Some(samples) = captured {
            sink(Packet {
                samples,
                discontinuity,
            });
            return;
        }
        let mut discontinuity = discontinuity || !silent;
        let mut left = len;
        while left > 0 {
            let samples = match &self.silence {
                Silence::F32(zeros) => Captured::F32(&zeros[..left.min(zeros.len())]),
                Silence::I32(zeros) => Captured::I32(&zeros[..left.min(zeros.len())]),
                Silence::I16(zeros) => Captured::I16(&zeros[..left.min(zeros.len())]),
            };
            left -= samples.len();
            sink(Packet {
                samples,
                discontinuity,
            });
            discontinuity = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `sink` was handed: the values and the flag of each call.
    fn read(
        reader: &Reader,
        data: *const u8,
        len: usize,
        silent: bool,
        discontinuity: bool,
    ) -> Vec<(Vec<f64>, bool)> {
        let mut seen = Vec::new();
        // SAFETY: every pointer the tests pass is null, ignored as silent,
        // or points into an array of at least `len` samples that outlives
        // the call.
        unsafe {
            reader.read(data, len, silent, discontinuity, &mut |packet| {
                let values = match packet.samples {
                    Captured::F32(s) => s.iter().map(|&v| f64::from(v)).collect(),
                    Captured::I32(s) => s.iter().map(|&v| f64::from(v)).collect(),
                    Captured::I16(s) => s.iter().map(|&v| f64::from(v)).collect(),
                };
                seen.push((values, packet.discontinuity));
            });
        }
        seen
    }

    #[test]
    fn a_packet_is_read_in_the_stream_format() {
        let floats = [0.25f32, -0.5, 1.0, 0.0];
        let reader = Reader::new(SampleKind::F32, 4);
        assert_eq!(
            read(&reader, floats.as_ptr().cast(), 4, false, false),
            [(vec![0.25, -0.5, 1.0, 0.0], false)]
        );
        let wide = [i32::MIN, 256, -256, i32::MAX];
        let reader = Reader::new(SampleKind::I32 { valid_bits: 24 }, 4);
        assert_eq!(
            read(&reader, wide.as_ptr().cast(), 4, false, true),
            [(wide.map(f64::from).to_vec(), true)]
        );
        let narrow = [i16::MIN, -1, 1, i16::MAX];
        let reader = Reader::new(SampleKind::I16, 4);
        assert_eq!(
            read(&reader, narrow.as_ptr().cast(), 4, false, false),
            [(narrow.map(f64::from).to_vec(), false)]
        );
    }

    #[test]
    fn a_silent_packet_is_zeros_whatever_its_data() {
        let floats = [9.0f32; 4];
        let reader = Reader::new(SampleKind::F32, 4);
        for data in [floats.as_ptr().cast(), std::ptr::null()] {
            assert_eq!(read(&reader, data, 4, true, false), [(vec![0.0; 4], false)]);
        }
    }

    #[test]
    fn long_silence_comes_a_period_at_a_time() {
        let reader = Reader::new(SampleKind::I16, 4);
        assert_eq!(
            read(&reader, std::ptr::null(), 10, true, true),
            [
                (vec![0.0; 4], true),
                (vec![0.0; 4], false),
                (vec![0.0; 2], false)
            ]
        );
        assert!(read(&reader, std::ptr::null(), 0, true, true).is_empty());
        // A reader asked for nothing still moves on.
        let reader = Reader::new(SampleKind::I16, 0);
        assert_eq!(read(&reader, std::ptr::null(), 2, true, false).len(), 2);
    }

    #[test]
    fn data_that_cannot_be_read_is_silence_and_a_discontinuity() {
        let words = [-1i32; 4];
        let reader = Reader::new(SampleKind::I32 { valid_bits: 32 }, 4);
        let misaligned = words.as_ptr().cast::<u8>().wrapping_add(1);
        for data in [std::ptr::null(), misaligned] {
            assert_eq!(read(&reader, data, 2, false, false), [(vec![0.0; 2], true)]);
        }
    }
}
