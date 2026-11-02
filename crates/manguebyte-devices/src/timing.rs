//! What a callback is told about when its audio is heard or was captured.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Timing of one block handed to a device callback.
///
/// The latencies are what the backend reports or predicts, so they are as
/// good as the driver behind it: exact on a good interface, an estimate on
/// many others, and `None` when the backend says nothing usable.
///
/// ```
/// use manguebyte_devices::CallbackInfo;
/// use std::time::Duration;
///
/// let info = CallbackInfo {
///     sample_rate: 48_000.0,
///     output_latency: Some(Duration::from_millis(10)),
///     ..CallbackInfo::default()
/// };
/// assert_eq!(info.output_latency_frames(), Some(480.0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CallbackInfo {
    /// Position of the block's first frame: how many frames the stream had
    /// handed over before it. It starts at zero and only counts up, across
    /// pauses too.
    pub frame: u64,
    /// Sample rate of the stream, in hertz.
    pub sample_rate: f64,
    /// The backend's clock when the block was asked for, from an origin
    /// that is fixed for the life of the stream but otherwise arbitrary.
    /// When a device hands over more than the largest block and the stream
    /// splits it, each piece is placed after the one before.
    pub stream_time: Duration,
    /// How long after [`stream_time`](Self::stream_time) the block's first
    /// frame reaches the device's output.
    pub output_latency: Option<Duration>,
    /// How long before [`stream_time`](Self::stream_time) the block's first
    /// input frame was captured. In a duplex stream it includes the reserve
    /// the stream keeps between the two devices, so that input latency plus
    /// output latency is the whole way from the input jack to the output.
    pub input_latency: Option<Duration>,
}

impl CallbackInfo {
    /// [`output_latency`](Self::output_latency) in frames.
    pub fn output_latency_frames(&self) -> Option<f64> {
        self.output_latency
            .map(|latency| latency.as_secs_f64() * self.sample_rate)
    }

    /// [`input_latency`](Self::input_latency) in frames.
    pub fn input_latency_frames(&self) -> Option<f64> {
        self.input_latency
            .map(|latency| latency.as_secs_f64() * self.sample_rate)
    }

    /// From the input jack to the output, when both latencies are known.
    pub fn round_trip_latency(&self) -> Option<Duration> {
        Some(self.input_latency? + self.output_latency?)
    }

    /// The same timing, `frames` later: what a piece of a split block is
    /// given. Real-time safe.
    pub(crate) fn advanced(&self, frames: usize) -> Self {
        let mut later = *self;
        later.frame += frames as u64;
        if self.sample_rate > 0.0 {
            later.stream_time = later
                .stream_time
                .saturating_add(frame_duration(frames, self.sample_rate));
        }
        later
    }
}

/// How long `frames` last at `sample_rate`, without the float conversion
/// panicking on nonsense.
pub(crate) fn frame_duration(frames: usize, sample_rate: f64) -> Duration {
    let seconds = frames as f64 / sample_rate;
    if seconds.is_finite() && seconds >= 0.0 {
        Duration::try_from_secs_f64(seconds).unwrap_or(Duration::ZERO)
    } else {
        Duration::ZERO
    }
}

/// The backend's clock as a duration from its origin.
pub(crate) fn since_origin(instant: cpal::StreamInstant) -> Duration {
    instant.saturating_duration_since(cpal::StreamInstant::ZERO)
}

/// Stream time and output latency from what an output backend reports.
pub(crate) fn output_timing(info: &cpal::OutputCallbackInfo) -> (Duration, Option<Duration>) {
    let stamp = info.timestamp();
    (
        since_origin(stamp.callback),
        stamp.playback.checked_duration_since(stamp.callback),
    )
}

/// Stream time and input latency from what an input backend reports.
pub(crate) fn input_timing(info: &cpal::InputCallbackInfo) -> (Duration, Option<Duration>) {
    let stamp = info.timestamp();
    (
        since_origin(stamp.callback),
        stamp.callback.checked_duration_since(stamp.capture),
    )
}

/// What a running stream shares with the threads that watch it: how far it
/// has got, and whether its device went away. Written by the audio thread
/// with plain atomic stores.
#[derive(Debug, Default)]
pub(crate) struct StreamHealth {
    frames: AtomicU64,
    lost: AtomicBool,
    restart: AtomicBool,
}

impl StreamHealth {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Real-time safe.
    pub(crate) fn advance(&self, frames: usize) {
        self.frames.fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub(crate) fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    pub(crate) fn mark_lost(&self) {
        self.lost.store(true, Ordering::Release);
    }

    pub(crate) fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// Real-time safe.
    pub(crate) fn request_restart(&self) {
        self.restart.store(true, Ordering::Release);
    }

    pub(crate) fn is_restart_requested(&self) -> bool {
        self.restart.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_block_moves_on_in_frames_and_time() {
        let info = CallbackInfo {
            frame: 1_000,
            sample_rate: 48_000.0,
            stream_time: Duration::from_secs(2),
            output_latency: Some(Duration::from_millis(5)),
            input_latency: None,
        };
        let later = info.advanced(480);
        assert_eq!(later.frame, 1_480);
        assert_eq!(later.stream_time, Duration::from_millis(2_010));
        assert_eq!(later.output_latency, info.output_latency);
        assert_eq!(info.round_trip_latency(), None);
        assert_eq!(info.output_latency_frames(), Some(240.0));
    }

    #[test]
    fn a_nonsense_rate_does_not_panic() {
        let info = CallbackInfo {
            sample_rate: f64::NAN,
            ..CallbackInfo::default()
        };
        assert_eq!(info.advanced(64).stream_time, Duration::ZERO);
        assert_eq!(frame_duration(10, 0.0), Duration::ZERO);
    }
}
