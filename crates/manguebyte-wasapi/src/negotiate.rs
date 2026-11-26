//! What opening a device decides, apart from the COM calls: the formats
//! tried and in what order, the period asked for, and what a failure code
//! means. Nothing here touches a device, so it is tested on every system.

// Only Windows' module calls these outside the tests.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::{Error, SampleKind};

/// The rates [`capabilities`](crate::capabilities) asks about.
pub(crate) const RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// The formats tried, best first: no conversion, then the widest integers.
pub(crate) const KINDS: [SampleKind; 4] = [
    SampleKind::F32,
    SampleKind::I32 { valid_bits: 32 },
    SampleKind::I32 { valid_bits: 24 },
    SampleKind::I16,
];

/// The `HRESULT`s that are told apart, as `Audioclient.h` and `winerror.h`
/// number them. A test on Windows compares them with the `windows` crate's.
pub(crate) mod code {
    pub(crate) const DEVICE_INVALIDATED: i32 = 0x8889_0004_u32 as i32;
    pub(crate) const UNSUPPORTED_FORMAT: i32 = 0x8889_0008_u32 as i32;
    pub(crate) const DEVICE_IN_USE: i32 = 0x8889_000A_u32 as i32;
    pub(crate) const EXCLUSIVE_MODE_NOT_ALLOWED: i32 = 0x8889_000E_u32 as i32;
    pub(crate) const SERVICE_NOT_RUNNING: i32 = 0x8889_0010_u32 as i32;
    pub(crate) const BUFFER_SIZE_NOT_ALIGNED: i32 = 0x8889_0019_u32 as i32;
    pub(crate) const ACCESS_DENIED: i32 = 0x8007_0005_u32 as i32;
}

/// What a failed call with this `HRESULT` means for the caller. `message`
/// is the system's text for it, often empty for the audio client's codes.
pub(crate) fn error_for(hresult: i32, message: &str) -> Error {
    let or = |plain: &str| {
        let message = message.trim();
        String::from(if message.is_empty() { plain } else { message })
    };
    match hresult {
        code::DEVICE_INVALIDATED => Error::DeviceLost,
        code::DEVICE_IN_USE => Error::DeviceBusy(or("another program holds it")),
        code::EXCLUSIVE_MODE_NOT_ALLOWED => Error::ExclusiveNotAllowed,
        code::ACCESS_DENIED => Error::AccessDenied,
        code::UNSUPPORTED_FORMAT => Error::FormatNotSupported(or("unsupported format")),
        other => Error::Os(format!(
            "{} (0x{:08X})",
            or("a call failed"),
            // The bits, as Windows writes an `HRESULT`.
            other as u32
        )),
    }
}

/// Whether this answer to "do you take this format?" is about the device as
/// a whole, so that asking about other formats is pointless. Any other
/// failure only says no to the format asked about: drivers answer that with
/// more than one code.
pub(crate) fn refuses_the_device(hresult: i32) -> bool {
    matches!(
        hresult,
        code::DEVICE_INVALIDATED
            | code::DEVICE_IN_USE
            | code::EXCLUSIVE_MODE_NOT_ALLOWED
            | code::SERVICE_NOT_RUNNING
            | code::ACCESS_DENIED
    )
}

/// Which structure describes a format to the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Header {
    /// `WAVEFORMATEXTENSIBLE`.
    Extensible,
    /// A plain PCM `WAVEFORMATEX`, with no extension: some drivers take
    /// 16-bit audio only so.
    Plain,
}

/// One format to try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) kind: SampleKind,
    pub(crate) header: Header,
}

fn extensible(kind: SampleKind) -> Candidate {
    Candidate {
        kind,
        header: Header::Extensible,
    }
}

const PLAIN_I16: Candidate = Candidate {
    kind: SampleKind::I16,
    header: Header::Plain,
};

/// The formats to try, in order: `preferred`, the one the driver says it
/// takes, first, since drivers do not always keep their word; then every
/// other one; 16-bit audio in a plain header last.
pub(crate) fn candidates(preferred: Option<SampleKind>) -> Vec<Candidate> {
    preferred
        .into_iter()
        .chain(KINDS.into_iter().filter(|&kind| Some(kind) != preferred))
        .map(extensible)
        .chain([PLAIN_I16])
        .collect()
}

/// A call that failed: its `HRESULT` and the system's text for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) hresult: i32,
    pub(crate) message: String,
}

/// Tries each of `candidates` until `attempt` takes one. A format the
/// driver does not support leads to the next; any other failure ends the
/// search. `what` names the rate and channel count, for the error when
/// none is taken.
pub(crate) fn negotiate<T>(
    candidates: &[Candidate],
    what: &str,
    mut attempt: impl FnMut(Candidate) -> Result<T, Failure>,
) -> Result<(Candidate, T), Error> {
    for &candidate in candidates {
        match attempt(candidate) {
            Ok(taken) => return Ok((candidate, taken)),
            Err(failure) if failure.hresult == code::UNSUPPORTED_FORMAT => {}
            Err(failure) => return Err(error_for(failure.hresult, &failure.message)),
        }
    }
    Err(Error::FormatNotSupported(what.to_owned()))
}

/// The numbers a format structure holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    /// Bits each sample takes in memory.
    pub(crate) container_bits: u16,
    /// Bits of them that carry the signal.
    pub(crate) valid_bits: u16,
    /// Bytes in a frame.
    pub(crate) block_align: u16,
    /// Bytes a second.
    pub(crate) bytes_per_second: u32,
    /// Front centre for one channel, front left and right for two, and
    /// straight through otherwise, as `cpal` does.
    pub(crate) channel_mask: u32,
}

/// How `kind` lies in memory with `channels` at `sample_rate`. Counts too
/// large for their fields saturate, and no driver takes them.
pub(crate) fn layout(kind: SampleKind, sample_rate: u32, channels: u16) -> Layout {
    let (container_bits, valid_bits) = match kind {
        SampleKind::F32 => (32, 32),
        SampleKind::I32 { valid_bits } => (32, valid_bits),
        SampleKind::I16 => (16, 16),
    };
    let block_align = channels.saturating_mul(container_bits / 8);
    Layout {
        container_bits,
        valid_bits,
        block_align,
        bytes_per_second: sample_rate.saturating_mul(u32::from(block_align)),
        channel_mask: match channels {
            1 => 0x4,
            2 => 0x3,
            _ => 0,
        },
    }
}

/// Frames in `period` (100 ns units) at `sample_rate`, to the nearest,
/// as WASAPI turns a period into whole frames.
pub(crate) fn frames_in(period: i64, sample_rate: u32) -> u32 {
    let frames = (i128::from(period.max(0)) * i128::from(sample_rate) + 5_000_000) / 10_000_000;
    u32::try_from(frames).unwrap_or(u32::MAX)
}

/// `frames` at `sample_rate` as a period in 100 ns units, rounded to the
/// nearest, which is what WASAPI expects to turn back into whole frames.
pub(crate) fn period_of(frames: u32, sample_rate: u32) -> i64 {
    if sample_rate == 0 {
        return 0;
    }
    let rate = i64::from(sample_rate);
    (i64::from(frames) * 10_000_000 + rate / 2) / rate
}

/// The period to ask a device for: `frames` at `sample_rate`, raised to
/// the device's shortest, `min`; with no `frames`, the shortest itself.
pub(crate) fn wanted_period(frames: Option<u32>, sample_rate: u32, min: i64) -> i64 {
    frames.map_or(min, |frames| period_of(frames, sample_rate).max(min))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(hresult: i32) -> Failure {
        Failure {
            hresult,
            message: String::new(),
        }
    }

    #[test]
    fn periods_and_frames_turn_into_each_other() {
        // 3 ms at 48 kHz is 144 frames; 10 ms is 480.
        assert_eq!(frames_in(30_000, 48_000), 144);
        assert_eq!(frames_in(100_000, 48_000), 480);
        // 128 frames at 44.1 kHz are 2.9025 ms, and back.
        assert_eq!(frames_in(29_025, 44_100), 128);
        assert_eq!(period_of(144, 48_000), 30_000);
        assert_eq!(period_of(128, 44_100), 29_025);
        assert_eq!(frames_in(period_of(256, 44_100), 44_100), 256);
        assert_eq!(period_of(10, 0), 0);
        assert_eq!(frames_in(-5, 48_000), 0);
        assert_eq!(frames_in(i64::MAX, u32::MAX), u32::MAX);
        // Whole frames survive the round trip at every rate asked about.
        for rate in RATES {
            for frames in [1, 7, 64, 144, 448, 1_024, 8_192] {
                assert_eq!(frames_in(period_of(frames, rate), rate), frames);
            }
        }
    }

    #[test]
    fn a_period_is_never_below_the_shortest() {
        assert_eq!(wanted_period(None, 48_000, 30_000), 30_000);
        assert_eq!(wanted_period(Some(64), 48_000, 30_000), 30_000);
        assert_eq!(wanted_period(Some(480), 48_000, 30_000), 100_000);
        assert_eq!(wanted_period(Some(0), 48_000, 30_000), 30_000);
        assert_eq!(wanted_period(Some(512), 0, 30_000), 30_000);
    }

    #[test]
    fn the_preferred_format_goes_first_and_plain_16_bit_last() {
        let order = candidates(None);
        assert_eq!(order.len(), 5);
        let kinds: Vec<SampleKind> = order[..4].iter().map(|c| c.kind).collect();
        assert_eq!(kinds, KINDS);
        assert!(order[..4].iter().all(|c| c.header == Header::Extensible));
        assert_eq!(order[4], PLAIN_I16);

        let order = candidates(Some(SampleKind::I16));
        assert_eq!(order[0], extensible(SampleKind::I16));
        assert_eq!(order[1], extensible(SampleKind::F32));
        assert_eq!(order.len(), 5, "no format is tried twice");
        assert_eq!(order[4], PLAIN_I16);
    }

    #[test]
    fn negotiation_passes_over_unsupported_formats_only() {
        let order = candidates(None);
        let mut tried = Vec::new();
        let (taken, value) = negotiate(&order, "48000 Hz", |candidate| {
            tried.push(candidate);
            if candidate.kind == (SampleKind::I32 { valid_bits: 24 }) {
                Ok(7)
            } else {
                Err(failure(code::UNSUPPORTED_FORMAT))
            }
        })
        .unwrap();
        assert_eq!((taken.kind, value), (SampleKind::I32 { valid_bits: 24 }, 7));
        assert_eq!(tried, order[..3]);

        let mut tries = 0;
        let error = negotiate::<()>(&order, "48000 Hz", |_| {
            tries += 1;
            Err(failure(code::DEVICE_IN_USE))
        })
        .unwrap_err();
        assert_eq!(tries, 1, "a device that is held is not asked again");
        assert!(matches!(error, Error::DeviceBusy(_)));

        let error = negotiate::<()>(&order, "48000 Hz, 3 channels", |_| {
            Err(failure(code::UNSUPPORTED_FORMAT))
        })
        .unwrap_err();
        assert_eq!(
            error,
            Error::FormatNotSupported(String::from("48000 Hz, 3 channels"))
        );
    }

    #[test]
    fn each_refusal_has_its_own_error() {
        assert_eq!(error_for(code::DEVICE_INVALIDATED, ""), Error::DeviceLost);
        assert_eq!(
            error_for(code::EXCLUSIVE_MODE_NOT_ALLOWED, "x"),
            Error::ExclusiveNotAllowed
        );
        assert_eq!(error_for(code::ACCESS_DENIED, ""), Error::AccessDenied);
        assert_eq!(
            error_for(code::DEVICE_IN_USE, "  "),
            Error::DeviceBusy(String::from("another program holds it"))
        );
        assert_eq!(
            error_for(code::SERVICE_NOT_RUNNING, ""),
            Error::Os(String::from("a call failed (0x88890010)"))
        );
        assert_eq!(
            error_for(code::BUFFER_SIZE_NOT_ALIGNED, "text"),
            Error::Os(String::from("text (0x88890019)"))
        );
        for hresult in [
            code::DEVICE_INVALIDATED,
            code::DEVICE_IN_USE,
            code::EXCLUSIVE_MODE_NOT_ALLOWED,
            code::SERVICE_NOT_RUNNING,
            code::ACCESS_DENIED,
        ] {
            assert!(refuses_the_device(hresult));
        }
        // `S_FALSE`, `E_INVALIDARG` and the plain "no" are about the format.
        for hresult in [1, 0x8007_0057_u32 as i32, code::UNSUPPORTED_FORMAT] {
            assert!(!refuses_the_device(hresult));
        }
    }

    #[test]
    fn a_format_lies_in_memory_as_its_kind_says() {
        let packed = layout(SampleKind::I32 { valid_bits: 24 }, 48_000, 2);
        assert_eq!((packed.container_bits, packed.valid_bits), (32, 24));
        assert_eq!((packed.block_align, packed.bytes_per_second), (8, 384_000));
        assert_eq!(packed.channel_mask, 0x3);
        let mono = layout(SampleKind::I16, 44_100, 1);
        assert_eq!((mono.block_align, mono.bytes_per_second), (2, 88_200));
        assert_eq!(mono.channel_mask, 0x4);
        assert_eq!(layout(SampleKind::F32, 96_000, 8).channel_mask, 0);
        // Nonsense does not overflow.
        let huge = layout(SampleKind::F32, u32::MAX, u16::MAX);
        assert_eq!(huge.block_align, u16::MAX);
        assert_eq!(huge.bytes_per_second, u32::MAX);
        assert_eq!(layout(SampleKind::F32, 48_000, 0).block_align, 0);
    }
}
