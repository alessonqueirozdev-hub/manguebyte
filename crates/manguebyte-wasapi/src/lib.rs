//! WASAPI exclusive-mode output, for `manguebyte-devices`.
//!
//! In shared mode, which is all `cpal` opens, every program's sound goes
//! through the Windows audio engine: it mixes them, converts them to the
//! device's format, and runs on its own period, about 10 ms, on top of the
//! program's buffer. In exclusive mode one program has the device to
//! itself: it writes the device's own format straight into the driver's
//! buffer, on a period it chooses, down to the device's minimum (often
//! 3 ms or less). Nothing else can play on that device meanwhile.
//!
//! `manguebyte-devices` forbids `unsafe` code, and WASAPI is COM, so the
//! calls live here, each with the reason it is sound written beside it.
//! Applications reach it through the device manager's
//! `"WASAPI exclusive"` host; this crate is its plumbing.
//!
//! - [`capabilities`] asks a device what it can do in exclusive mode: its
//!   rates and its periods.
//! - [`start`] opens it, starts a thread of its own with the "Pro Audio"
//!   priority, and calls `render` once a period to fill the device's
//!   buffer, in the device's [`Samples`] format.
//!
//! Every COM object a stream uses is made, used and released on that one
//! thread. On other systems every function says
//! [`Error::Unsupported`].
//!
//! It only plays. Exclusive capture is not here.

use std::fmt;
use std::time::Duration;

#[cfg(windows)]
mod exclusive;

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Not on this system, or not for this device.
    Unsupported(String),
    /// No active output device has this identifier.
    DeviceNotFound(String),
    /// Another program holds the device, or Windows does not let programs
    /// take it ("Allow applications to take exclusive control" is off in
    /// the device's properties).
    DeviceBusy(String),
    /// The device takes none of the formats tried at this rate and channel
    /// count.
    FormatNotSupported(String),
    /// The device went away while it played.
    DeviceLost,
    /// Anything else the system said.
    Os(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(why) => write!(f, "WASAPI exclusive mode is not available: {why}"),
            Self::DeviceNotFound(id) => write!(f, "no active output device {id}"),
            Self::DeviceBusy(why) => write!(f, "the device cannot be taken exclusively: {why}"),
            Self::FormatNotSupported(why) => write!(f, "no format the device takes: {why}"),
            Self::DeviceLost => f.write_str("the device went away"),
            Self::Os(why) => write!(f, "WASAPI: {why}"),
        }
    }
}

impl std::error::Error for Error {}

/// What a device can do in exclusive mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// The rates, in hertz, at which some format is accepted with the
    /// channel count asked about.
    pub sample_rates: Vec<u32>,
    /// The shortest period the device runs on, in 100 ns units.
    pub min_period: i64,
    /// The period it runs on when not told, in 100 ns units.
    pub default_period: i64,
}

impl Capabilities {
    /// The shortest period in frames at `sample_rate`, to the nearest.
    pub fn min_period_frames(&self, sample_rate: u32) -> u32 {
        frames_in(self.min_period, sample_rate)
    }
}

/// How a device stores a sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    /// 32-bit float.
    F32,
    /// 32-bit integer; `valid_bits` of them carry the signal (24 on most
    /// devices), the rest are below it and ignored.
    I32 {
        /// How many of the 32 bits the device uses.
        valid_bits: u16,
    },
    /// 16-bit integer.
    I16,
}

/// What to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// Sample rate, in hertz.
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: u16,
    /// The period, in frames. `None` asks for the device's shortest. A
    /// period below it is raised to it.
    pub period_frames: Option<u32>,
}

/// What was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    /// Sample rate, in hertz.
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: u16,
    /// How each sample is stored.
    pub kind: SampleKind,
    /// Frames in each buffer `render` fills: one period.
    pub period_frames: u32,
    /// How long after a buffer is handed over its first frame is heard,
    /// as the driver reports it.
    pub latency: Duration,
}

/// One period of the device's buffer to fill, interleaved, in the
/// device's own format.
#[derive(Debug)]
pub enum Samples<'a> {
    /// [`SampleKind::F32`].
    F32(&'a mut [f32]),
    /// [`SampleKind::I32`]: full-scale 32-bit values; the bits below the
    /// device's valid ones are dropped by it.
    I32(&'a mut [i32]),
    /// [`SampleKind::I16`].
    I16(&'a mut [i16]),
}

/// A running exclusive stream. Dropping it stops the device and joins its
/// thread; the device is free for other programs again.
pub struct Stream {
    /// Held for its `Drop`, which stops the device and joins the thread.
    #[cfg(windows)]
    _running: exclusive::Running,
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stream").finish_non_exhaustive()
    }
}

/// The device's identifier as WASAPI knows it: `cpal` writes the same
/// one after `"wasapi:"`, and either form is taken.
fn endpoint_id(id: &str) -> &str {
    id.strip_prefix("wasapi:").unwrap_or(id)
}

/// Frames in `period` (100 ns units) at `sample_rate`, to the nearest,
/// as WASAPI turns a period into whole frames.
fn frames_in(period: i64, sample_rate: u32) -> u32 {
    let frames = (i128::from(period.max(0)) * i128::from(sample_rate) + 5_000_000) / 10_000_000;
    u32::try_from(frames).unwrap_or(u32::MAX)
}

/// `frames` at `sample_rate` as a period in 100 ns units, rounded to the
/// nearest, which is what WASAPI expects to turn back into whole frames.
fn period_of(frames: u32, sample_rate: u32) -> i64 {
    if sample_rate == 0 {
        return 0;
    }
    let rate = i64::from(sample_rate);
    (i64::from(frames) * 10_000_000 + rate / 2) / rate
}

/// Asks the output device `id` what it can do in exclusive mode with
/// `channels` channels. It opens no stream.
pub fn capabilities(id: &str, channels: u16) -> Result<Capabilities, Error> {
    #[cfg(windows)]
    {
        exclusive::capabilities(endpoint_id(id), channels)
    }
    #[cfg(not(windows))]
    {
        let _ = (id, channels);
        Err(Error::Unsupported(String::from("WASAPI is Windows'")))
    }
}

/// Opens the output device `id` exclusively and starts it.
///
/// `render` runs on the stream's own thread once a period, with the
/// buffer to fill and how long the stream has run. It must not allocate,
/// lock or block. It is handed a buffer of silence to overwrite; leaving it
/// untouched plays silence. `on_error` runs on the same thread, at most
/// once, when the device fails or goes away; the stream then stops.
///
/// Returns once the device runs, with the format it runs in.
pub fn start<R, E>(
    id: &str,
    request: Request,
    render: R,
    on_error: E,
) -> Result<(Stream, Format), Error>
where
    R: FnMut(Samples<'_>, Duration) + Send + 'static,
    E: FnMut(Error) + Send + 'static,
{
    #[cfg(windows)]
    {
        let (inner, format) =
            exclusive::start(endpoint_id(id).to_owned(), request, render, on_error)?;
        Ok((Stream { _running: inner }, format))
    }
    #[cfg(not(windows))]
    {
        let _ = (id, request, render, on_error);
        Err(Error::Unsupported(String::from("WASAPI is Windows'")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cpal_prefix_is_taken_off() {
        assert_eq!(
            endpoint_id("wasapi:{0.0.0.00000000}.{x}"),
            "{0.0.0.00000000}.{x}"
        );
        assert_eq!(endpoint_id("{0.0.0.00000000}.{x}"), "{0.0.0.00000000}.{x}");
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
    }
}
