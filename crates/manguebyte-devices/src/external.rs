//! A seam for audio backends that live outside this crate.

use std::sync::Arc;

use manguebyte_core::{AudioBuffer, NoDenormals};

use crate::backend::{
    Backend, EngineCallback, ErrorSink, OpenedEndpoint, OpenedFormat, Started, StreamRequest,
    list_system_midi,
};
#[cfg(feature = "midi")]
use crate::backend::{MidiSender, connect_system_midi_input, connect_system_midi_output};
#[cfg(feature = "midi")]
use crate::midi::{MidiError, MidiSink};
use crate::timing::StreamHealth;
use crate::{
    CallbackInfo, DeviceError, DeviceInfo, DeviceSnapshot, DeviceSummary, Direction, HostInfo,
    StreamSettings,
};

/// A source of audio devices that a [`DeviceManager`] can run on, in place
/// of the system's: a driver model this crate does not speak, a network
/// transport, a simulator.
///
/// A device manager reaches the system's devices through `cpal`. A crate
/// of its own can put another driver model behind the same manager by
/// implementing this trait and handing it to
/// [`DeviceManager::with_audio_backend`]. The manager then plans, opens,
/// watches and restarts streams on it as it does on the system's devices,
/// and saved setups, fallbacks and device events work the same. MIDI still
/// comes from the system's ports.
///
/// The backend's side of the bargain:
///
/// - It lists its devices ([`snapshot`](Self::snapshot)) and describes
///   each one ([`device_info`](Self::device_info)).
/// - It starts one stream at a time ([`start`](Self::start)): it opens
///   what the [`BackendRequest`] names, tells the manager the format it
///   got, and receives a [`BackendCallback`] made for that format, which it
///   runs on its audio thread with planar `f32` buffers.
/// - It tells the manager what happens to the stream through the
///   [`StreamStatus`] it is handed: a device that went away, or a request
///   to open the stream again because the driver's settings changed. The
///   manager hears of both on its next poll as
///   [`DeviceEvent::StreamLost`](crate::DeviceEvent::StreamLost), and
///   [`follow`](crate::DeviceManager::follow) restarts the stream, off the
///   audio thread.
/// - Dropping the [`BackendStream`] it returns stops the stream and drops
///   the callback, which hands the application's callback back to the
///   manager.
///
/// Every method is called off the audio thread, from whichever thread uses
/// the manager or polls it. The crate's `tests/external.rs` has a complete
/// backend in a hundred lines.
///
/// [`DeviceManager`]: crate::DeviceManager
/// [`DeviceManager::with_audio_backend`]: crate::DeviceManager::with_audio_backend
pub trait AudioBackend: Send + Sync + 'static {
    /// The backend's name, as a [`DeviceSetup`](crate::DeviceSetup) names
    /// its host. It is the only host the manager lists.
    fn host_name(&self) -> String;

    /// The audio devices connected now, and the defaults. Called on every
    /// poll, so it should be cheap. The MIDI fields are ignored: the
    /// manager lists the system's MIDI ports itself.
    fn snapshot(&self) -> Result<DeviceSnapshot, DeviceError>;

    /// Everything about one device, with its configurations, as the
    /// manager needs to choose a sample rate, channel counts and a buffer
    /// size for it.
    fn device_info(&self, direction: Direction, id: &str) -> Result<DeviceInfo, DeviceError>;

    /// Whether the backend can start a second stream, on another device,
    /// while one runs. A device manager with an
    /// [`EngineFormat`](crate::EngineFormat) then crossfades from the old
    /// device to the new one; otherwise it fades the old one out, stops
    /// it, and fades the new one in. `false` unless overridden.
    fn supports_concurrent_streams(&self) -> bool {
        false
    }

    /// Opens the devices of `request` and starts them.
    ///
    /// Once the devices are open, call `make` exactly once with the format
    /// they run in; it returns the callback to run. Run it on the audio
    /// thread for every block, through [`BackendCallback::process`]. If
    /// anything fails after `make` was called, drop the callback before
    /// returning the error.
    ///
    /// Report what happens to the stream through `status`.
    fn start(
        &self,
        request: &BackendRequest,
        make: &mut dyn FnMut(BackendFormat) -> BackendCallback,
        status: StreamStatus,
    ) -> Result<BackendStream, DeviceError>;
}

/// What an [`AudioBackend`] is asked to open.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendRequest {
    /// The device to record from, if any.
    pub input: Option<BackendEndpoint>,
    /// The device to play to, if any.
    pub output: Option<BackendEndpoint>,
    /// The largest block the application's callback takes.
    /// [`BackendCallback::process`] splits larger blocks itself, so the
    /// backend may hand it whatever its hardware delivers.
    pub max_block_size: usize,
}

impl BackendRequest {
    /// A request, for testing a backend outside a device manager.
    pub fn new(
        input: Option<BackendEndpoint>,
        output: Option<BackendEndpoint>,
        max_block_size: usize,
    ) -> Self {
        Self {
            input,
            output,
            max_block_size,
        }
    }
}

/// One side of a [`BackendRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendEndpoint {
    /// The device, by the identifier the backend's snapshot gave it.
    pub id: String,
    /// What to run it with. `None` leaves a value to the device.
    pub settings: StreamSettings,
}

impl BackendEndpoint {
    /// An endpoint, for testing a backend outside a device manager.
    pub fn new(id: impl Into<String>, settings: StreamSettings) -> Self {
        Self {
            id: id.into(),
            settings,
        }
    }
}

/// The format a backend's devices run in, as it tells the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendFormat {
    /// Sample rate, in hertz.
    pub sample_rate: u32,
    /// Input channels; zero with no input.
    pub input_channels: usize,
    /// Output channels; zero with no output.
    pub output_channels: usize,
}

impl BackendFormat {
    /// A format.
    pub fn new(sample_rate: u32, input_channels: usize, output_channels: usize) -> Self {
        Self {
            sample_rate,
            input_channels,
            output_channels,
        }
    }
}

/// The application's callback, as a backend runs it.
///
/// It splits blocks longer than the application takes, hands it a silent
/// output buffer every time, keeps subnormal numbers from slowing it down,
/// and counts the frames that went through, which the manager watches to
/// notice a stream that stopped.
pub struct BackendCallback {
    callback: EngineCallback,
    format: BackendFormat,
    max_block_size: usize,
    input: Arc<StreamHealth>,
    output: Arc<StreamHealth>,
}

impl std::fmt::Debug for BackendCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendCallback")
            .field("format", &self.format)
            .field("max_block_size", &self.max_block_size)
            .finish_non_exhaustive()
    }
}

impl BackendCallback {
    /// A callback that runs `callback`, for testing a backend outside a
    /// device manager.
    ///
    /// # Panics
    ///
    /// Panics if `max_block_size` is zero.
    pub fn from_fn<F>(format: BackendFormat, max_block_size: usize, callback: F) -> Self
    where
        F: FnMut(&AudioBuffer<f32>, &mut AudioBuffer<f32>, &CallbackInfo) + Send + 'static,
    {
        assert!(max_block_size > 0, "max_block_size must be non-zero");
        Self {
            callback: Box::new(callback),
            format,
            max_block_size,
            input: StreamHealth::new(),
            output: StreamHealth::new(),
        }
    }

    /// The format the callback was prepared for.
    pub fn format(&self) -> BackendFormat {
        self.format
    }

    /// The largest block the application's callback takes.
    pub fn max_block_size(&self) -> usize {
        self.max_block_size
    }

    /// Runs the callback on one block, on the audio thread. Real-time
    /// safe: it does not allocate, lock or block.
    ///
    /// `input` holds the captured audio, with the input's channels (none
    /// without an input); `output` receives the audio to play, with the
    /// output's channels (none without an output). Both must hold the same
    /// number of frames. `input` may be changed: it is the backend's
    /// scratch. `info` is the timing of the block's first frame.
    ///
    /// A block longer than [`max_block_size`](Self::max_block_size) is
    /// handed over in pieces, each with its own timing. Buffers of
    /// different lengths are the backend's mistake, and play silence.
    pub fn process(
        &mut self,
        input: &mut AudioBuffer<f32>,
        output: &mut AudioBuffer<f32>,
        info: &CallbackInfo,
    ) {
        let _quiet = NoDenormals::new();
        output.clear();
        let frames = output.num_frames();
        if input.num_frames() != frames {
            return;
        }
        let callback = &mut self.callback;
        if frames <= self.max_block_size {
            callback(input, output, info);
        } else {
            let mut info = *info;
            let mut start = 0;
            while start < frames {
                let end = (start + self.max_block_size).min(frames);
                input.windowed(start..end, |input| {
                    output.windowed(start..end, |output| callback(input, output, &info));
                });
                info = info.advanced(end - start);
                start = end;
            }
        }
        self.input.advance(frames);
        self.output.advance(frames);
    }
}

/// What a backend reports about a running stream, from any thread.
///
/// Clones report about the same stream.
#[derive(Clone)]
pub struct StreamStatus {
    input: Arc<StreamHealth>,
    output: Arc<StreamHealth>,
    /// The side the manager watches for this stream's loss: the output when
    /// there is one, so that a duplex stream is reported, and restarted,
    /// once.
    watched: Direction,
    errors: ErrorSink,
}

impl std::fmt::Debug for StreamStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamStatus")
            .field("lost", &self.is_lost())
            .field("restart_requested", &self.is_restart_requested())
            .finish_non_exhaustive()
    }
}

impl StreamStatus {
    /// A status whose errors go to `handler`, for testing a backend
    /// outside a device manager.
    pub fn new(handler: impl Fn(DeviceError) + Send + Sync + 'static) -> Self {
        Self::watching(Direction::Output, Arc::new(handler))
    }

    fn watching(watched: Direction, errors: ErrorSink) -> Self {
        Self {
            input: StreamHealth::new(),
            output: StreamHealth::new(),
            watched,
            errors,
        }
    }

    fn health(&self) -> &StreamHealth {
        match self.watched {
            Direction::Input => &self.input,
            Direction::Output => &self.output,
        }
    }

    /// Says that the stream's device went away and the stream will not
    /// produce audio again. Real-time safe.
    pub fn report_lost(&self) {
        self.health().mark_lost();
    }

    /// Asks for the stream to be stopped and opened again, as a driver
    /// asks when its sample rate, buffer size or other settings changed.
    /// Real-time safe: it only sets a flag, which the manager sees on its
    /// next poll.
    pub fn request_restart(&self) {
        self.health().request_restart();
    }

    /// Reports an error. Not real-time safe: the handler may allocate,
    /// lock or print, so call it off the audio thread.
    pub fn report_error(&self, error: DeviceError) {
        (self.errors)(error);
    }

    /// Whether [`report_lost`](Self::report_lost) was called.
    pub fn is_lost(&self) -> bool {
        self.health().is_lost()
    }

    /// Whether [`request_restart`](Self::request_restart) was called.
    pub fn is_restart_requested(&self) -> bool {
        self.health().is_restart_requested()
    }
}

/// A stream an [`AudioBackend`] started.
///
/// Dropping it must stop the stream and drop the [`BackendCallback`].
pub struct BackendStream {
    stream: Box<dyn Send>,
    sample_rate: u32,
    input: Option<(DeviceSummary, StreamSettings)>,
    output: Option<(DeviceSummary, StreamSettings)>,
}

impl std::fmt::Debug for BackendStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendStream")
            .field("sample_rate", &self.sample_rate)
            .field("input", &self.input)
            .field("output", &self.output)
            .finish_non_exhaustive()
    }
}

impl BackendStream {
    /// A running stream at `sample_rate`. `stream` is whatever keeps it
    /// running; dropping it stops the stream.
    pub fn new(stream: impl Send + 'static, sample_rate: u32) -> Self {
        Self {
            stream: Box::new(stream),
            sample_rate,
            input: None,
            output: None,
        }
    }

    /// Says which device the stream records from, and how it runs.
    #[must_use]
    pub fn with_input(mut self, device: DeviceSummary, settings: StreamSettings) -> Self {
        self.input = Some((device, settings));
        self
    }

    /// Says which device the stream plays to, and how it runs.
    #[must_use]
    pub fn with_output(mut self, device: DeviceSummary, settings: StreamSettings) -> Self {
        self.output = Some((device, settings));
        self
    }

    /// The device and settings of one side.
    pub fn endpoint(&self, direction: Direction) -> Option<&(DeviceSummary, StreamSettings)> {
        match direction {
            Direction::Input => self.input.as_ref(),
            Direction::Output => self.output.as_ref(),
        }
    }

    /// The sample rate, in hertz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// An [`AudioBackend`] as the device manager sees backends.
pub(crate) struct External {
    backend: Arc<dyn AudioBackend>,
}

impl External {
    pub(crate) fn new(backend: Arc<dyn AudioBackend>) -> Self {
        Self { backend }
    }
}

impl Backend for External {
    fn host_name(&self) -> String {
        self.backend.host_name()
    }

    fn hosts(&self) -> Vec<HostInfo> {
        vec![HostInfo {
            name: self.backend.host_name(),
            is_default: true,
        }]
    }

    fn select_host(&self, name: Option<&str>) -> Result<(), DeviceError> {
        match name {
            Some(name) if !name.trim().eq_ignore_ascii_case(&self.backend.host_name()) => {
                Err(DeviceError::HostUnavailable(name.to_owned()))
            }
            _ => Ok(()),
        }
    }

    fn snapshot(&self) -> Result<DeviceSnapshot, DeviceError> {
        let mut snapshot = self.backend.snapshot()?;
        list_system_midi(&mut snapshot);
        Ok(snapshot)
    }

    fn device_info(&self, direction: Direction, id: &str) -> Result<DeviceInfo, DeviceError> {
        self.backend.device_info(direction, id)
    }

    fn concurrent_streams(&self) -> bool {
        self.backend.supports_concurrent_streams()
    }

    fn start(
        &self,
        request: &StreamRequest,
        make: &mut dyn FnMut(OpenedFormat) -> EngineCallback,
        on_error: ErrorSink,
    ) -> Result<Started, DeviceError> {
        let watched = if request.output.is_some() {
            Direction::Output
        } else {
            Direction::Input
        };
        let status = StreamStatus::watching(watched, on_error);
        let max_block_size = request.duplex.max_block_size.max(1);
        let endpoint = |side: &Option<crate::backend::Endpoint>| {
            side.as_ref().map(|endpoint| BackendEndpoint {
                id: endpoint.id.clone(),
                settings: endpoint.settings,
            })
        };
        let public = BackendRequest {
            input: endpoint(&request.input),
            output: endpoint(&request.output),
            max_block_size,
        };
        let mut prepared = None;
        let stream = self.backend.start(
            &public,
            &mut |format: BackendFormat| {
                let opened = OpenedFormat {
                    sample_rate: format.sample_rate,
                    input_channels: format.input_channels,
                    output_channels: format.output_channels,
                };
                prepared = Some(opened);
                BackendCallback {
                    callback: make(opened),
                    format,
                    max_block_size,
                    input: Arc::clone(&status.input),
                    output: Arc::clone(&status.output),
                }
            },
            status.clone(),
        )?;
        let side = |opened: Option<(DeviceSummary, StreamSettings)>, health: &Arc<StreamHealth>| {
            opened.map(|(device, settings)| OpenedEndpoint {
                device,
                settings,
                health: Arc::clone(health),
            })
        };
        let channels = |side: &Option<(DeviceSummary, StreamSettings)>| {
            side.as_ref()
                .and_then(|(_, settings)| settings.channels)
                .map_or(0, usize::from)
        };
        let format = prepared.unwrap_or(OpenedFormat {
            sample_rate: stream.sample_rate,
            input_channels: channels(&stream.input),
            output_channels: channels(&stream.output),
        });
        Ok(Started {
            stream: stream.stream,
            format,
            input: side(stream.input, &status.input),
            output: side(stream.output, &status.output),
        })
    }

    #[cfg(feature = "midi")]
    fn connect_midi_input(&self, id: &str, sink: MidiSink) -> Result<Box<dyn Send>, MidiError> {
        connect_system_midi_input(id, sink)
    }

    #[cfg(feature = "midi")]
    fn connect_midi_output(&self, id: &str) -> Result<Box<dyn MidiSender>, MidiError> {
        connect_system_midi_output(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_block_is_handed_over_in_pieces_that_follow_each_other() {
        let format = BackendFormat::new(1_000, 1, 2);
        let mut seen = Vec::new();
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut callback = BackendCallback::from_fn(format, 4, move |input, output, info| {
            assert_eq!(output.peak(), 0.0, "the output starts silent");
            let _ = sender.send((input.num_frames(), info.frame, input.channel(0)[0]));
            for channel in output.iter_channels_mut() {
                channel.fill(1.0);
            }
        });
        let mut input = AudioBuffer::new(1, 10);
        for (frame, sample) in input.channel_mut(0).iter_mut().enumerate() {
            *sample = frame as f32;
        }
        let mut output = AudioBuffer::new(2, 10);
        output.channel_mut(0).fill(9.0);
        let info = CallbackInfo {
            frame: 100,
            sample_rate: 1_000.0,
            ..CallbackInfo::default()
        };
        callback.process(&mut input, &mut output, &info);
        seen.extend(receiver.try_iter());
        assert_eq!(seen, [(4, 100, 0.0), (4, 104, 4.0), (2, 108, 8.0)]);
        assert_eq!(output.channel(1), &[1.0; 10]);
        assert_eq!(callback.output.frames(), 10);

        // Buffers that disagree play silence instead of panicking.
        let mut short = AudioBuffer::new(1, 3);
        callback.process(&mut short, &mut output, &info);
        assert_eq!(output.peak(), 0.0);
    }

    #[test]
    fn a_status_marks_only_the_side_the_manager_watches() {
        let status = StreamStatus::watching(Direction::Input, Arc::new(|_| {}));
        status.request_restart();
        assert!(status.is_restart_requested());
        assert!(status.input.is_restart_requested());
        assert!(!status.output.is_restart_requested());
        status.report_lost();
        assert!(status.is_lost());
        assert!(format!("{status:?}").contains("lost: true"));
    }
}
