//! WASAPI exclusive-mode output and input, for `manguebyte-devices`.
//!
//! In shared mode, which is all `cpal` opens, every program's sound goes
//! through the Windows audio engine: it mixes them, converts them to the
//! device's format, and runs on its own period, about 10 ms, on top of the
//! program's buffer. In exclusive mode one program has the device to
//! itself: it moves the device's own format straight to or from the
//! driver's buffer, on a period it chooses, down to the device's minimum
//! (often 3 ms or less). Nothing else can use that device meanwhile.
//!
//! `manguebyte-devices` forbids `unsafe` code, and WASAPI is COM, so the
//! calls live here, each with the reason it is sound written beside it.
//! Applications reach it through the device manager's
//! `"WASAPI exclusive"` host; this crate is its plumbing.
//!
//! - [`capabilities`] asks a device what it can do in exclusive mode: its
//!   rates and its periods, or why it refuses to say.
//! - [`start`] opens an output, starts a thread of its own with the "Pro
//!   Audio" priority, and calls `render` once a period to fill the device's
//!   buffer, in the device's [`Samples`] format.
//! - [`start_capture`] opens an input the same way and calls `capture` once
//!   a period with what the device recorded, as a [`Packet`].
//!
//! Every COM object a stream uses is made, used and released on that one
//! thread. A program that records and plays at once starts one of each and
//! carries the audio between their two threads itself, as
//! `manguebyte-devices` does. On other systems every function says
//! [`Error::Unsupported`].

use std::fmt;
use std::time::Duration;

#[cfg(windows)]
mod exclusive;
mod negotiate;
mod packet;

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Not on this system, or not for this device.
    Unsupported(String),
    /// No active device has this identifier.
    DeviceNotFound(String),
    /// Another program holds the device.
    DeviceBusy(String),
    /// Windows does not let programs take the device: "Allow applications
    /// to take exclusive control" is off in its properties.
    ExclusiveNotAllowed,
    /// Windows denies this program access to the device, as its microphone
    /// privacy setting does for inputs.
    AccessDenied,
    /// The device takes none of the formats tried at this rate and channel
    /// count.
    FormatNotSupported(String),
    /// The device went away.
    DeviceLost,
    /// Anything else the system said.
    Os(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(why) => write!(f, "WASAPI exclusive mode is not available: {why}"),
            Self::DeviceNotFound(id) => write!(f, "no active audio device {id}"),
            Self::DeviceBusy(why) => write!(f, "the device cannot be taken exclusively: {why}"),
            Self::ExclusiveNotAllowed => f.write_str(
                "the device does not allow exclusive control (a setting in its Windows properties)",
            ),
            Self::AccessDenied => f.write_str(
                "Windows denies access to the device (for an input, see the microphone privacy settings)",
            ),
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
        negotiate::frames_in(self.min_period, sample_rate)
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
    /// Frames in each buffer `render` fills or `capture` is handed: one
    /// period.
    pub period_frames: u32,
    /// The stream's latency as the driver reports it, or one period when it
    /// reports none: for an output, how long after a buffer is handed over
    /// its first frame is heard.
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

/// What a capture device recorded, interleaved, in the device's own
/// format.
#[derive(Debug, Clone, Copy)]
pub enum Captured<'a> {
    /// [`SampleKind::F32`].
    F32(&'a [f32]),
    /// [`SampleKind::I32`]: full-scale 32-bit values; the bits below the
    /// device's valid ones carry nothing.
    I32(&'a [i32]),
    /// [`SampleKind::I16`].
    I16(&'a [i16]),
}

impl Captured<'_> {
    /// How many samples there are: frames times channels.
    pub fn len(&self) -> usize {
        match self {
            Self::F32(samples) => samples.len(),
            Self::I32(samples) => samples.len(),
            Self::I16(samples) => samples.len(),
        }
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One delivery of a capture stream, normally one period.
#[derive(Debug, Clone, Copy)]
pub struct Packet<'a> {
    /// The recorded samples. All zeros when the driver marks the packet
    /// silent.
    pub samples: Captured<'a>,
    /// Audio was lost before this packet, as the driver says (the first
    /// packet of a stream may say so), or because the packet itself could
    /// not be read and zeros stand for it.
    pub discontinuity: bool,
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
#[cfg_attr(not(windows), allow(dead_code))]
fn endpoint_id(id: &str) -> &str {
    id.strip_prefix("wasapi:").unwrap_or(id)
}

/// Asks the device `id`, an output or an input, what it can do in exclusive
/// mode with `channels` channels. It opens no stream, so it cannot tell
/// whether another program holds the device.
///
/// A device that takes no format with that many channels has no
/// [`sample_rates`](Capabilities::sample_rates). One that refuses as a
/// whole gives the reason as the error, as far as the driver says it
/// without being opened.
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
/// [`Error::Unsupported`] when `id` is an input.
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

/// Opens the input device `id` exclusively and starts it.
///
/// `capture` runs on the stream's own thread once a period, with what the
/// device recorded and how long the stream has run. It must not allocate,
/// lock or block. The samples are the driver's own buffer, lent for that
/// call only. `on_error` runs on the same thread, at most once, when the
/// device fails or goes away; the stream then stops.
///
/// Returns once the device runs, with the format it runs in.
/// [`Error::Unsupported`] when `id` is an output: exclusive mode has no
/// loopback capture of an output.
pub fn start_capture<C, E>(
    id: &str,
    request: Request,
    capture: C,
    on_error: E,
) -> Result<(Stream, Format), Error>
where
    C: FnMut(Packet<'_>, Duration) + Send + 'static,
    E: FnMut(Error) + Send + 'static,
{
    #[cfg(windows)]
    {
        let (inner, format) =
            exclusive::start_capture(endpoint_id(id).to_owned(), request, capture, on_error)?;
        Ok((Stream { _running: inner }, format))
    }
    #[cfg(not(windows))]
    {
        let _ = (id, request, capture, on_error);
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

    /// No device has this identifier: on Windows the system is asked and
    /// says so, through the stream's thread too; elsewhere there is no
    /// WASAPI.
    #[test]
    fn a_device_that_does_not_exist_opens_nothing() {
        let id = "{0.0.1.00000000}.{00000000-0000-0000-0000-000000000000}";
        let request = Request {
            sample_rate: 48_000,
            channels: 2,
            period_frames: None,
        };
        let errors = [
            capabilities(id, 2).map(|_| ()),
            start(id, request, |_, _| {}, |_| {}).map(|_| ()),
            start_capture(id, request, |_, _| {}, |_| {}).map(|_| ()),
        ];
        for error in errors.map(Result::unwrap_err) {
            if cfg!(windows) {
                assert_eq!(error, Error::DeviceNotFound(id.to_owned()));
            } else {
                assert!(matches!(error, Error::Unsupported(_)), "{error:?}");
            }
        }
    }

    #[test]
    fn captured_samples_are_counted_in_any_format() {
        assert_eq!(Captured::F32(&[0.0; 3]).len(), 3);
        assert_eq!(Captured::I32(&[0; 2]).len(), 2);
        assert!(Captured::I16(&[]).is_empty());
    }
}
