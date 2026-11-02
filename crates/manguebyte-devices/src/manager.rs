//! Opening the devices a setup names, keeping them running, and following
//! the system when devices come and go.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use manguebyte_core::AudioBuffer;
use manguebyte_core::spsc::{self, Producer};

#[cfg(feature = "midi")]
use crate::backend::MidiSender;
use crate::backend::{
    Backend, Endpoint, EngineCallback, ErrorSink, OpenedEndpoint, OpenedFormat, Started,
    StreamRequest, SystemBackend,
};
use crate::hotplug::{DeviceEvent, DeviceWatcher, Poller, WatchSlot, WatchedStream};
use crate::hotswap::{
    Core, DEFAULT_CROSSFADE, Edge, EdgeInit, EdgeParts, EdgeShared, EngineFormat, Incoming, Retire,
    Successor, SwapKind, SwapRecord, SwapReport,
};
#[cfg(feature = "midi")]
use crate::midi::{
    MidiError, MidiInputMessage, MidiProtocol, MidiReceiver, MidiSink, MidiTransmitter,
    SharedOutput, SharedOutputThread, Timed, merged_receiver, midi_output_queue,
};
#[cfg(feature = "midi")]
use crate::setup::DeviceRef;
use crate::setup::{Fallback, Plan, plan};
use crate::{
    AudioHost, CallbackInfo, DeviceError, DeviceInfo, DeviceSetup, DeviceSnapshot, DeviceSummary,
    Direction, DuplexSettings, HostInfo, LossCause, MockBackend, RestoreReport, StreamSettings,
};

/// The largest block a device manager hands its callback unless told
/// otherwise, in frames.
pub const DEFAULT_MAX_BLOCK_SIZE: usize = 1_024;

/// How long a stopping stream may take to give its callback back.
const RETURN_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a swap waits, beyond the fade itself, for an old device to
/// finish fading out before it is stopped anyway: a device that stalled
/// does not hold the swap up for longer.
const FADE_TIMEOUT: Duration = Duration::from_millis(250);

/// What a setup would open, as [`DeviceManager::preview`] works it out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SetupPreview {
    /// The output device and what it would be asked for; `None` when no
    /// output would run.
    pub output: Option<(DeviceSummary, StreamSettings)>,
    /// The input device and what it would be asked for; `None` when no
    /// input would run.
    pub input: Option<(DeviceSummary, StreamSettings)>,
    /// The MIDI input ports that would be open.
    pub midi_inputs: Vec<DeviceSummary>,
    /// What would differ from the setup, as the restore report would say.
    pub fallbacks: Vec<Fallback>,
}

impl SetupPreview {
    /// The sample rate the devices would run at.
    pub fn sample_rate(&self) -> Option<u32> {
        self.output
            .iter()
            .chain(&self.input)
            .find_map(|(_, settings)| settings.sample_rate)
    }
}

/// The format a callback will run with, as told to
/// [`AudioCallback::prepare`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamFormat {
    /// Sample rate, in hertz.
    pub sample_rate: f64,
    /// The largest block [`AudioCallback::process`] will be handed.
    pub max_block_size: usize,
    /// Input channels; zero with no input.
    pub input_channels: usize,
    /// Output channels; zero with no output.
    pub output_channels: usize,
}

/// One block of audio, as a device manager hands it to its callback.
#[non_exhaustive]
pub struct DeviceIo<'a> {
    /// Captured audio, with the input device's channels; no channels when
    /// there is no input.
    pub input: &'a AudioBuffer<f32>,
    /// Silent audio to fill, with the output device's channels; no
    /// channels when there is no output. As long as `input`.
    pub output: &'a mut AudioBuffer<f32>,
    /// When the block is heard and was captured.
    pub info: CallbackInfo,
    #[cfg(feature = "midi")]
    pub(crate) midi: &'a mut MidiReceiver,
}

impl DeviceIo<'_> {
    /// Frames in this block.
    pub fn num_frames(&self) -> usize {
        self.output.num_frames()
    }

    /// Hands over the MIDI that arrived on every open input port since the
    /// last block, each message with its frame inside this block (see
    /// [`MidiReceiver`]). Call it once per block.
    ///
    /// Real-time safe.
    #[cfg(feature = "midi")]
    pub fn read_midi(&mut self, handle: impl FnMut(u32, MidiInputMessage)) {
        let frames = self.output.num_frames();
        self.midi.read(frames, self.info.sample_rate, handle);
    }

    /// Hands over the MIDI that arrived since the last block as the
    /// framework's events, read through `input` so that MPE becomes
    /// per-note events (see [`MidiReceiver::read_events`]). Call it once
    /// per block, instead of [`read_midi`](Self::read_midi).
    ///
    /// Real-time safe.
    #[cfg(feature = "midi")]
    pub fn read_midi_events(
        &mut self,
        input: &mut manguebyte_dsp::note_input::NoteInput,
        events: &mut manguebyte_dsp::event::EventBuffer,
    ) {
        let frames = self.output.num_frames();
        self.midi
            .read_events(frames, self.info.sample_rate, input, events);
    }
}

/// What a [`DeviceManager`] runs.
///
/// A closure taking `&mut DeviceIo<'_>` is one; a type that needs to know
/// the format before it runs implements the trait.
///
/// The manager keeps the callback across restarts: when the devices
/// change, it stops the stream, gets the callback back, calls
/// [`prepare`](Self::prepare) with the new format and starts again. A
/// callback started with
/// [`DeviceManager::start_engine`] is prepared once, with its
/// [`EngineFormat`], and never again: the devices change under it.
pub trait AudioCallback: Send + 'static {
    /// Called before the stream starts, on the thread that starts it, with
    /// the format it will run with. Allocate here.
    fn prepare(&mut self, format: &StreamFormat) {
        let _ = format;
    }

    /// Processes one block, on the audio thread. Must not allocate, lock,
    /// block or do I/O.
    fn process(&mut self, io: &mut DeviceIo<'_>);

    /// Called after the stream stopped, on the thread that stopped it.
    fn release(&mut self) {}
}

impl<F> AudioCallback for F
where
    F: FnMut(&mut DeviceIo<'_>) + Send + 'static,
{
    fn process(&mut self, io: &mut DeviceIo<'_>) {
        self(io);
    }
}

/// The callback, and what it reads MIDI from, while no stream holds them.
pub(crate) struct Parked {
    pub(crate) callback: Box<dyn AudioCallback>,
    #[cfg(feature = "midi")]
    pub(crate) midi: MidiReceiver,
}

/// What a stream runs. When the stream drops it, it sends the callback
/// back to the manager.
struct Engine {
    parked: Option<Parked>,
    returns: Sender<Parked>,
}

impl Engine {
    fn run(
        &mut self,
        input: &AudioBuffer<f32>,
        output: &mut AudioBuffer<f32>,
        info: &CallbackInfo,
    ) {
        let Some(parked) = &mut self.parked else {
            return;
        };
        let mut io = DeviceIo {
            input,
            output,
            info: *info,
            #[cfg(feature = "midi")]
            midi: &mut parked.midi,
        };
        parked.callback.process(&mut io);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(parked) = self.parked.take() {
            let _ = self.returns.send(parked);
        }
    }
}

struct Running {
    stream: Box<dyn Send>,
    format: StreamFormat,
    input: Option<OpenedEndpoint>,
    output: Option<OpenedEndpoint>,
    /// The edge the stream runs, with an engine format.
    edge: Option<EdgeLink>,
}

impl Running {
    /// Whether the backend said a device of the stream went away.
    fn is_lost(&self) -> bool {
        self.output
            .iter()
            .chain(&self.input)
            .any(|endpoint| endpoint.health.is_lost())
    }

    /// Whether the stream's edge has faded out, or its device is gone, so
    /// that it can be dropped.
    fn is_finished(&self) -> bool {
        self.is_lost() || self.edge.as_ref().is_none_or(|edge| edge.shared.is_faded())
    }
}

/// How the manager reaches a running stream's edge.
struct EdgeLink {
    shared: Arc<EdgeShared>,
    commands: Producer<Retire>,
    /// The device's sample rate.
    rate: u32,
}

/// What a manager with an engine format keeps.
struct EngineMode {
    format: EngineFormat,
    fade: Duration,
    /// The engine, while no stream runs it.
    core: Option<Box<Core>>,
    /// Where an edge dropped with the engine sends it.
    returns: (Sender<Box<Core>>, Receiver<Box<Core>>),
    /// Streams fading out while the running one fades in, oldest first.
    retiring: Vec<Running>,
    /// Whether the running stream's device is known to be gone, so that
    /// waiting for it to fade out is pointless.
    dead: bool,
    /// Whether a stream has run the engine yet: later ones fade in.
    started: bool,
    last_swap: Option<SwapRecord>,
}

/// Opens the input, output and MIDI devices a [`DeviceSetup`] names, runs
/// one callback on them, and restores the setup in a later run with
/// fallbacks for whatever is missing.
///
/// ```no_run
/// use manguebyte_devices::{DeviceChoice, DeviceIo, DeviceManager, DeviceSetup};
///
/// // What the settings file held, or the defaults on a first run.
/// let saved: Option<String> = None;
/// let setup = saved
///     .and_then(|text| text.parse().ok())
///     .unwrap_or_else(|| DeviceSetup::default().with_input(DeviceChoice::Default));
///
/// let mut manager = DeviceManager::new();
/// let report = manager.start(&setup, |io: &mut DeviceIo<'_>| {
///     // Pass the first input channel through to every output channel.
///     if io.input.num_channels() > 0 {
///         for channel in io.output.iter_channels_mut() {
///             channel.copy_from_slice(io.input.channel(0));
///         }
///     }
/// })?;
/// for fallback in &report.fallbacks {
///     println!("{fallback}");
/// }
///
/// // Later, when the program quits:
/// let to_save = manager.setup().to_string();
/// # let _ = to_save;
/// # Ok::<(), manguebyte_devices::DeviceError>(())
/// ```
///
/// # Following the system
///
/// Devices come and go while the program runs. [`poll_devices`] (from a
/// timer of the application's own) or [`watch`] (a thread of its own)
/// report what changed as [`DeviceEvent`]s; [`follow`] reacts to one: it
/// restarts on the saved device when it comes back, on the default when
/// the stream's device is lost, and opens MIDI ports of the setup as they
/// appear.
///
/// [`poll_devices`]: Self::poll_devices
/// [`watch`]: Self::watch
/// [`follow`]: Self::follow
///
/// # Changing devices without stopping
///
/// [`start`](Self::start) prepares the callback for each device it opens,
/// so changing devices stops it and prepares it again.
/// [`start_engine`](Self::start_engine) instead fixes the callback's
/// format once, as an [`EngineFormat`], and prepares it once. From then
/// on [`apply`](Self::apply), [`restart`](Self::restart) and
/// [`follow`](Self::follow) change the devices under the running callback,
/// in the middle of a song: it is never stopped or prepared again, and it
/// runs on, frame after frame, as if nothing had happened.
///
/// Where the backend can run two streams at once and the output device
/// changes, the new device fades in while the old one fades out, both
/// playing; otherwise (one ASIO driver per process, the same device opened
/// again, a driver's reset request) the old device fades out, is closed,
/// and the new one fades in, with only the time the backend takes to
/// close and open in between. A device at another rate than the engine's
/// is fed through a resampler; one at the engine's rate with its output
/// channels is fed directly, with nothing copied or delayed.
/// [`last_swap`](Self::last_swap) says how the last change went. The
/// [`hotswap`](crate::hotswap) module tells the whole story.
///
/// ```
/// use manguebyte_devices::{
///     DeviceChoice, DeviceIo, DeviceManager, DeviceRef, DeviceSetup, Direction, EngineFormat,
///     MockBackend, SwapKind,
/// };
///
/// let mock = MockBackend::new();
/// mock.set_concurrent_streams(true);
/// mock.add_device(Direction::Output, MockBackend::device("a", "Speakers", 2, 48_000));
/// mock.add_device(Direction::Output, MockBackend::device("b", "Headphones", 2, 44_100));
///
/// let mut manager = DeviceManager::headless(&mock);
/// let format = EngineFormat::new(48_000, 256, 0, 2);
/// manager.start_engine(&DeviceSetup::default(), format, |io: &mut DeviceIo<'_>| {
///     io.output.channel_mut(0).fill(0.5);
/// })?;
///
/// // Mid-song, to the headphones, at another rate.
/// let headphones = DeviceChoice::Device(DeviceRef::new("b", "Headphones"));
/// manager.apply(&DeviceSetup::default().with_output(headphones))?;
/// assert_eq!(manager.last_swap().unwrap().kind, SwapKind::Crossfade);
/// assert_eq!(manager.engine_format(), Some(format));
/// # Ok::<(), manguebyte_devices::DeviceError>(())
/// ```
pub struct DeviceManager {
    backend: Arc<dyn Backend>,
    setup: DeviceSetup,
    max_block_size: usize,
    drift_correction: bool,
    crossfade: Duration,
    errors: ErrorSink,
    callback: Option<Box<dyn AudioCallback>>,
    returns: (Sender<Parked>, Receiver<Parked>),
    running: Option<Running>,
    engine: Option<EngineMode>,
    watched: WatchSlot,
    generation: u64,
    poller: Poller,
    #[cfg(feature = "midi")]
    midi_receiver: Option<MidiReceiver>,
    #[cfg(feature = "midi")]
    midi_shared: Arc<Mutex<Producer<Timed>>>,
    #[cfg(feature = "midi")]
    midi_inputs: Vec<(DeviceSummary, Box<dyn Send>)>,
    /// The open MIDI output port, as the shared output has it, so that it
    /// can be lent out.
    #[cfg(feature = "midi")]
    midi_output: Option<DeviceSummary>,
    /// The MIDI output port itself, shared with the thread that sends it
    /// what the audio thread queues.
    #[cfg(feature = "midi")]
    midi_shared_output: Arc<Mutex<SharedOutput>>,
    /// That thread, once a callback asked for a transmitter.
    #[cfg(feature = "midi")]
    midi_sending: Option<SharedOutputThread>,
}

impl std::fmt::Debug for DeviceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceManager")
            .field("host", &self.backend.host_name())
            .field("setup", &self.setup)
            .field("running", &self.is_running())
            .finish_non_exhaustive()
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceManager {
    /// A manager for the system's devices, on its default host. Nothing is
    /// opened until [`start`](Self::start).
    pub fn new() -> Self {
        Self::with_backend(Arc::new(SystemBackend::new(AudioHost::default_host())))
    }

    /// A manager for the devices of a [`MockBackend`], for tests and for
    /// running without hardware.
    pub fn headless(mock: &MockBackend) -> Self {
        Self::with_backend(Arc::new(mock.clone()))
    }

    /// A manager for the devices of a backend outside this crate (see
    /// [`AudioBackend`](crate::AudioBackend)). Its host is the only one
    /// listed, MIDI ports are the system's, and everything else works as
    /// on the system's devices: setups, fallbacks, events and
    /// [`follow`](Self::follow), which also restarts a stream whose backend
    /// asked for it ([`LossCause::RestartRequested`](crate::LossCause::RestartRequested)).
    ///
    /// ```
    /// use manguebyte_devices::{
    ///     AudioBackend, BackendCallback, BackendFormat, BackendRequest, BackendStream,
    ///     DeviceError, DeviceInfo, DeviceManager, DeviceSnapshot, Direction, StreamStatus,
    /// };
    ///
    /// /// A backend with no devices at all.
    /// struct Empty;
    ///
    /// impl AudioBackend for Empty {
    ///     fn host_name(&self) -> String {
    ///         String::from("Empty")
    ///     }
    ///     fn snapshot(&self) -> Result<DeviceSnapshot, DeviceError> {
    ///         Ok(DeviceSnapshot::default())
    ///     }
    ///     fn device_info(&self, _: Direction, id: &str) -> Result<DeviceInfo, DeviceError> {
    ///         Err(DeviceError::DeviceNotFound(id.to_owned()))
    ///     }
    ///     fn start(
    ///         &self,
    ///         request: &BackendRequest,
    ///         _: &mut dyn FnMut(BackendFormat) -> BackendCallback,
    ///         _: StreamStatus,
    ///     ) -> Result<BackendStream, DeviceError> {
    ///         let id = request.output.as_ref().map(|output| output.id.clone());
    ///         Err(DeviceError::DeviceNotFound(id.unwrap_or_default()))
    ///     }
    /// }
    ///
    /// let manager = DeviceManager::with_audio_backend(Empty);
    /// assert_eq!(manager.host_name(), "Empty");
    /// assert!(manager.devices()?.outputs.is_empty());
    /// # Ok::<(), DeviceError>(())
    /// ```
    pub fn with_audio_backend(backend: impl crate::AudioBackend) -> Self {
        Self::with_backend(Arc::new(crate::external::External::new(Arc::new(backend))))
    }

    fn with_backend(backend: Arc<dyn Backend>) -> Self {
        let watched: WatchSlot = Arc::new(Mutex::new(None));
        #[cfg(feature = "midi")]
        let (midi_shared, midi_receiver) = merged_receiver();
        Self {
            poller: Poller::new(Arc::clone(&backend), Arc::clone(&watched)),
            backend,
            setup: DeviceSetup::default(),
            max_block_size: DEFAULT_MAX_BLOCK_SIZE,
            drift_correction: true,
            crossfade: DEFAULT_CROSSFADE,
            errors: Arc::new(|error| eprintln!("manguebyte: audio stream error: {error}")),
            callback: None,
            returns: mpsc::channel(),
            running: None,
            engine: None,
            watched,
            generation: 0,
            #[cfg(feature = "midi")]
            midi_receiver: Some(midi_receiver),
            #[cfg(feature = "midi")]
            midi_shared,
            #[cfg(feature = "midi")]
            midi_inputs: Vec::new(),
            #[cfg(feature = "midi")]
            midi_output: None,
            #[cfg(feature = "midi")]
            midi_shared_output: Arc::new(Mutex::new(SharedOutput::new())),
            #[cfg(feature = "midi")]
            midi_sending: None,
        }
    }

    /// The audio hosts that can be chosen.
    pub fn hosts(&self) -> Vec<HostInfo> {
        self.backend.hosts()
    }

    /// The name of the host in use.
    pub fn host_name(&self) -> String {
        self.backend.host_name()
    }

    /// What is connected now, on the host in use.
    pub fn devices(&self) -> Result<DeviceSnapshot, DeviceError> {
        self.backend.snapshot()
    }

    /// Everything about one device of the host in use, with its
    /// configurations, for a settings dialog.
    pub fn device_info(&self, direction: Direction, id: &str) -> Result<DeviceInfo, DeviceError> {
        self.backend.device_info(direction, id)
    }

    /// Sets the largest block the callback is handed. Takes effect when
    /// the stream next starts. A callback started with
    /// [`start_engine`](Self::start_engine) is handed blocks of up to its
    /// [`EngineFormat::max_block_size`] instead.
    ///
    /// # Panics
    ///
    /// Panics if `frames` is zero.
    pub fn set_max_block_size(&mut self, frames: usize) {
        assert!(frames > 0, "the block size must be non-zero");
        self.max_block_size = frames;
    }

    /// Whether a stream joining an input and an output that are separate
    /// devices follows the drift between their clocks (on by default). See
    /// [`DuplexSettings::drift_correction`]. Takes effect when the stream
    /// next starts.
    pub fn set_drift_correction(&mut self, on: bool) {
        self.drift_correction = on;
    }

    /// Where stream errors go; standard error by default. Called from the
    /// backend's threads, never from the callback's path. Takes effect when
    /// the stream next starts.
    pub fn set_error_handler(&mut self, handler: impl Fn(DeviceError) + Send + Sync + 'static) {
        self.errors = Arc::new(handler);
    }

    /// Opens the devices of `setup` and starts running `callback` on them.
    ///
    /// Whatever the setup names that cannot be had is replaced, as the
    /// returned report says: a missing device by one with its name or by the
    /// default, a rate or channel count the device cannot do by one it can.
    /// When nothing at all can be opened the report says so and no stream
    /// runs; [`follow`](Self::follow) starts one once a device appears.
    ///
    /// A stream already running is stopped first, and its callback dropped.
    pub fn start(
        &mut self,
        setup: &DeviceSetup,
        callback: impl AudioCallback,
    ) -> Result<RestoreReport, DeviceError> {
        self.stop();
        drop(self.leave_engine());
        if let Some(mut old) = self.callback.take() {
            old.release();
        }
        self.callback = Some(Box::new(callback));
        self.setup = setup.clone();
        self.open()
    }

    /// Opens the devices of `setup` and runs `callback` on them at a fixed
    /// format, so that the devices can change later without stopping it.
    ///
    /// The callback is prepared now, once, with `format`, and is never
    /// prepared again while it runs: whatever devices the setup opens,
    /// now and after [`apply`](Self::apply), [`restart`](Self::restart) or
    /// [`follow`](Self::follow), it runs at `format`'s rate, with its
    /// channels and blocks of at most its largest block, and its
    /// [`CallbackInfo::frame`] counts on across every change. See
    /// [Changing devices without stopping](Self#changing-devices-without-stopping).
    ///
    /// Fallbacks are reported as by [`start`](Self::start). A stream
    /// already running is stopped first, and its callback dropped.
    pub fn start_engine(
        &mut self,
        setup: &DeviceSetup,
        format: EngineFormat,
        callback: impl AudioCallback,
    ) -> Result<RestoreReport, DeviceError> {
        self.stop();
        drop(self.leave_engine());
        if let Some(mut old) = self.callback.take() {
            old.release();
        }
        let mut callback: Box<dyn AudioCallback> = Box::new(callback);
        callback.prepare(&format.stream_format());
        #[cfg(feature = "midi")]
        let midi = {
            if self.midi_receiver.is_none() {
                self.renew_midi();
            }
            let Some(mut midi) = self.midi_receiver.take() else {
                return Err(DeviceError::NoCallback);
            };
            midi.restart();
            midi
        };
        self.engine = Some(EngineMode {
            format,
            fade: self.crossfade,
            core: Some(Core::new(Parked {
                callback,
                #[cfg(feature = "midi")]
                midi,
            })),
            returns: mpsc::channel(),
            retiring: Vec::new(),
            dead: false,
            started: false,
            last_swap: None,
        });
        self.setup = setup.clone();
        self.open()
    }

    /// The format the callback runs at, when it was started with
    /// [`start_engine`](Self::start_engine).
    pub fn engine_format(&self) -> Option<EngineFormat> {
        self.engine.as_ref().map(|engine| engine.format)
    }

    /// Sets how long a device fades in and out when a callback started
    /// with [`start_engine`](Self::start_engine) changes devices;
    /// [`DEFAULT_CROSSFADE`](crate::DEFAULT_CROSSFADE) unless told
    /// otherwise. Zero switches at once, with a click.
    pub fn set_crossfade(&mut self, fade: Duration) {
        self.crossfade = fade;
        if let Some(engine) = &mut self.engine {
            engine.fade = fade;
        }
    }

    /// How the last change of devices under a callback started with
    /// [`start_engine`](Self::start_engine) went. The silence it reports
    /// is known once the new device has played.
    pub fn last_swap(&self) -> Option<SwapReport> {
        self.engine
            .as_ref()?
            .last_swap
            .as_ref()
            .map(SwapRecord::report)
    }

    /// Changes the setup, and restarts the stream on it with the same
    /// callback. With an [`EngineFormat`], the new devices take over from
    /// the old ones without stopping the callback.
    pub fn apply(&mut self, setup: &DeviceSetup) -> Result<RestoreReport, DeviceError> {
        self.setup = setup.clone();
        self.open()
    }

    /// Applies `setup` and gives the callback started with
    /// [`start_engine`](Self::start_engine) a new format, for a change the
    /// devices' edge should not hide, such as following a device to
    /// another sample rate instead of resampling it.
    ///
    /// With the format the callback runs at, this is
    /// [`apply`](Self::apply). Otherwise the running device fades out and
    /// stops, the callback is released and prepared again with `format`,
    /// and the setup's devices start with it and fade in: the same
    /// callback goes on, and [`last_swap`](Self::last_swap) reports a
    /// [`SwapKind::Gap`]. If the new devices cannot be started, the
    /// callback keeps `format` and waits, as after a failed
    /// [`apply`](Self::apply).
    ///
    /// Returns [`DeviceError::NoCallback`] for a manager that was not
    /// started with [`start_engine`](Self::start_engine).
    pub fn apply_with_format(
        &mut self,
        setup: &DeviceSetup,
        format: EngineFormat,
    ) -> Result<RestoreReport, DeviceError> {
        let Some(engine) = &self.engine else {
            return Err(DeviceError::NoCallback);
        };
        if engine.format == format {
            return self.apply(setup);
        }
        self.finish_crossfades(true);
        let faded = if self
            .running
            .as_ref()
            .is_some_and(|running| !running.is_lost())
        {
            self.fade_out_running().map(|old| (old, Instant::now()))
        } else {
            None
        };
        self.collect_core();
        let Some(engine) = &mut self.engine else {
            return Err(DeviceError::NoCallback);
        };
        let Some(core) = &mut engine.core else {
            return Err(DeviceError::NoCallback);
        };
        core.parked.callback.release();
        core.parked.callback.prepare(&format.stream_format());
        engine.format = format;
        engine.dead = false;
        self.setup = setup.clone();
        self.open_engine_after(faded)
    }

    /// Restarts the stream on the setup, for example after its device was
    /// lost: the saved devices are looked for again, with the same
    /// fallbacks as at the start.
    pub fn restart(&mut self) -> Result<RestoreReport, DeviceError> {
        self.open()
    }

    /// Stops the stream. The callback is kept for [`restart`](Self::restart),
    /// and MIDI ports stay open. The stream stops at once, without a fade.
    pub fn stop(&mut self) {
        if self.engine.is_some() {
            self.collect_core();
            return;
        }
        let Some(running) = self.running.take() else {
            return;
        };
        if let Ok(mut watched) = self.watched.lock() {
            *watched = None;
        }
        drop(running.stream);
        match self.returns.1.recv_timeout(RETURN_TIMEOUT) {
            Ok(parked) => self.park(parked),
            Err(_) => {
                // The backend never dropped the callback: it is gone, and so
                // is the MIDI receiver it held. Start MIDI afresh.
                #[cfg(feature = "midi")]
                self.renew_midi();
            }
        }
    }

    /// Stops the stream and gives the callback back.
    pub fn take_callback(&mut self) -> Option<Box<dyn AudioCallback>> {
        self.stop();
        if self.engine.is_some() {
            return self.leave_engine();
        }
        self.callback.take()
    }

    /// The setup asked for, to save and restore in a later run. It names
    /// the devices that were asked for, not those a fallback put in their
    /// place, so that they are used again when they come back.
    pub fn setup(&self) -> &DeviceSetup {
        &self.setup
    }

    /// Whether a stream is running.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The format of the running stream.
    pub fn format(&self) -> Option<StreamFormat> {
        self.running.as_ref().map(|running| running.format)
    }

    /// The device the running stream uses in one direction.
    pub fn running_device(&self, direction: Direction) -> Option<&DeviceSummary> {
        self.endpoint(direction).map(|endpoint| &endpoint.device)
    }

    /// The settings the running stream's device has in one direction.
    pub fn running_settings(&self, direction: Direction) -> Option<StreamSettings> {
        self.endpoint(direction).map(|endpoint| endpoint.settings)
    }

    fn endpoint(&self, direction: Direction) -> Option<&OpenedEndpoint> {
        let running = self.running.as_ref()?;
        match direction {
            Direction::Input => running.input.as_ref(),
            Direction::Output => running.output.as_ref(),
        }
    }

    /// Lists the devices now and reports what changed since the last call,
    /// including a running stream whose device was lost. The first call only
    /// learns what is there. Call it from a timer of the application's
    /// own, about every [`DEFAULT_POLL_INTERVAL`](crate::DEFAULT_POLL_INTERVAL).
    ///
    /// With an [`EngineFormat`], it also closes the old stream of a
    /// crossfade once it has faded out.
    pub fn poll_devices(&mut self) -> Vec<DeviceEvent> {
        self.finish_crossfades(false);
        self.poller.poll()
    }

    /// Starts a thread that lists the devices every `interval` and hands
    /// what changed to `handler`, as [`poll_devices`](Self::poll_devices)
    /// would. Hand the events back to the manager's thread, through a
    /// channel for example, and pass them to [`follow`](Self::follow).
    pub fn watch<H>(&self, interval: Duration, handler: H) -> std::io::Result<DeviceWatcher>
    where
        H: FnMut(DeviceEvent) + Send + 'static,
    {
        DeviceWatcher::spawn(
            Poller::new(Arc::clone(&self.backend), Arc::clone(&self.watched)),
            interval,
            handler,
        )
    }

    /// Reacts to a change among the devices: restarts the stream when its
    /// device was lost, or when the setup would now pick other devices than
    /// those running (the saved device came back, the default changed while
    /// the setup asks for the default, a first device appeared), and opens
    /// or closes the setup's MIDI ports as they come and go.
    ///
    /// Returns `None` when the event changes nothing here, and what the
    /// restart reported otherwise.
    pub fn follow(&mut self, event: &DeviceEvent) -> Option<Result<RestoreReport, DeviceError>> {
        match event {
            DeviceEvent::StreamLost {
                direction,
                device,
                cause,
            } => {
                // An event about a stream that has been replaced since,
                // following an earlier event, needs nothing more.
                let running = self.running_device(*direction);
                let current = self.running.is_none() || running.is_some_and(|d| d.id == device.id);
                if current && *cause != LossCause::RestartRequested {
                    // Its device plays nothing more: there is no fade-out
                    // to wait for.
                    if let Some(engine) = &mut self.engine {
                        engine.dead = true;
                    }
                }
                current.then(|| self.restart())
            }
            DeviceEvent::DeviceAdded { .. }
            | DeviceEvent::DeviceRemoved { .. }
            | DeviceEvent::DefaultChanged { .. } => {
                if self.callback.is_none() && self.engine.is_none() && self.running.is_none() {
                    return None;
                }
                let planned = self.plan_now().ok()?;
                let wanted = |side: &Option<(DeviceSummary, StreamSettings)>| {
                    side.as_ref().map(|(device, _)| device.id.clone())
                };
                let running = |direction| self.running_device(direction).map(|d| d.id.clone());
                if wanted(&planned.output) == running(Direction::Output)
                    && wanted(&planned.input) == running(Direction::Input)
                {
                    return None;
                }
                Some(self.restart())
            }
            #[cfg(feature = "midi")]
            DeviceEvent::MidiPortAdded { .. } | DeviceEvent::MidiPortRemoved { .. } => {
                let planned = self.plan_now().ok()?;
                // What the setup's MIDI ports fell back to, such as a port
                // that went away.
                let mut report = RestoreReport {
                    fallbacks: planned
                        .fallbacks
                        .iter()
                        .filter(|fallback| {
                            matches!(
                                fallback,
                                Fallback::MidiPortMissing { .. }
                                    | Fallback::MidiPortReplaced { .. }
                            )
                        })
                        .cloned()
                        .collect(),
                };
                let before = self.open_midi_ports();
                self.connect_midi(&planned, &mut report);
                (self.open_midi_ports() != before).then_some(Ok(report))
            }
            _ => None,
        }
    }

    fn plan_now(&self) -> Result<Plan, DeviceError> {
        self.plan_for(&self.setup)
    }

    fn plan_for(&self, setup: &DeviceSetup) -> Result<Plan, DeviceError> {
        let snapshot = self.backend.snapshot()?;
        let backend = Arc::clone(&self.backend);
        Ok(plan(setup, &snapshot, &|direction, id| {
            backend.device_info(direction, id).ok()
        }))
    }

    /// What [`apply`](Self::apply) would open for `setup` now, worked out
    /// without opening or changing anything: the devices with their
    /// settings, the MIDI input ports, and the fallbacks it would report,
    /// for a caller that must choose something, such as an engine format,
    /// before it applies the setup.
    ///
    /// It plans on the host in use: a setup that names another host is
    /// planned as if it named this one. A backend that loads one driver
    /// at a time may be unable to describe another device while a stream
    /// runs, and then plans that device away, as [`apply`](Self::apply)
    /// would before it stops the stream and plans again.
    pub fn preview(&self, setup: &DeviceSetup) -> Result<SetupPreview, DeviceError> {
        let planned = self.plan_for(setup)?;
        Ok(SetupPreview {
            output: planned.output,
            input: planned.input,
            midi_inputs: planned.midi_inputs,
            fallbacks: planned.fallbacks,
        })
    }

    fn park(&mut self, parked: Parked) {
        let mut callback = parked.callback;
        callback.release();
        self.callback = Some(callback);
        #[cfg(feature = "midi")]
        {
            self.midi_receiver = Some(parked.midi);
        }
    }

    /// Selects the setup's host and plans its devices, noting in `report`
    /// what had to change, and connects its MIDI ports.
    fn plan_open(&mut self, report: &mut RestoreReport) -> Result<Plan, DeviceError> {
        let wanted_host = self.setup.host.clone();
        if self.backend.select_host(wanted_host.as_deref()).is_err() {
            let _ = self.backend.select_host(None);
            report.fallbacks.push(Fallback::HostUnavailable {
                wanted: wanted_host.unwrap_or_default(),
                used: self.backend.host_name(),
            });
        }
        let planned = self.plan_now()?;
        report.fallbacks.extend(planned.fallbacks.iter().cloned());
        #[cfg(feature = "midi")]
        self.connect_midi(&planned, report);
        Ok(planned)
    }

    /// The stream to start for `planned`.
    fn request(planned: &Plan, max_block_size: usize, drift_correction: bool) -> StreamRequest {
        let endpoint = |side: &Option<(DeviceSummary, StreamSettings)>| {
            side.as_ref().map(|(device, settings)| Endpoint {
                id: device.id.clone(),
                settings: *settings,
            })
        };
        StreamRequest {
            input: endpoint(&planned.input),
            output: endpoint(&planned.output),
            duplex: DuplexSettings::new(max_block_size).with_drift_correction(drift_correction),
        }
    }

    fn open(&mut self) -> Result<RestoreReport, DeviceError> {
        if self.engine.is_some() {
            return self.open_engine();
        }
        self.stop();
        if self.callback.is_none() {
            return Err(DeviceError::NoCallback);
        }
        let mut report = RestoreReport::default();
        let planned = self.plan_open(&mut report)?;
        let Some(callback) = self.callback.take() else {
            return Err(DeviceError::NoCallback);
        };

        let mut request = Self::request(&planned, self.max_block_size, self.drift_correction);
        if request.input.is_none() && request.output.is_none() {
            self.callback = Some(callback);
            return Ok(report);
        }

        match self.try_start(&request, callback) {
            Ok(()) => Ok(report),
            Err((error, callback)) => {
                let Some(callback) = callback else {
                    return Err(error);
                };
                // An input that will not open must not keep the output
                // silent: try again without it.
                let (Some(_), Some(_), Some((input, _))) =
                    (&request.input, &request.output, &planned.input)
                else {
                    self.callback = Some(callback);
                    return Err(error);
                };
                request.input = None;
                match self.try_start(&request, callback) {
                    Ok(()) => {
                        report.fallbacks.push(crate::setup::Fallback::InputDropped {
                            device: input.clone(),
                            reason: error.to_string(),
                        });
                        Ok(report)
                    }
                    Err((error, callback)) => {
                        self.callback = callback;
                        Err(error)
                    }
                }
            }
        }
    }

    /// Makes a stream that started the running one, and has the pollers
    /// watch it.
    fn register(&mut self, started: Started, format: StreamFormat, edge: Option<EdgeLink>) {
        self.generation += 1;
        let endpoints = [
            (Direction::Output, started.output.as_ref()),
            (Direction::Input, started.input.as_ref()),
        ]
        .into_iter()
        .filter_map(|(direction, endpoint)| {
            let endpoint = endpoint?;
            Some((
                direction,
                endpoint.device.clone(),
                Arc::clone(&endpoint.health),
            ))
        })
        .collect();
        if let Ok(mut watched) = self.watched.lock() {
            *watched = Some(WatchedStream {
                generation: self.generation,
                endpoints,
            });
        }
        self.running = Some(Running {
            stream: started.stream,
            format,
            input: started.input,
            output: started.output,
            edge,
        });
    }

    /// Starts `request` with `callback`. On failure, hands the callback
    /// back with the error, unless the backend lost it.
    #[allow(clippy::type_complexity)]
    fn try_start(
        &mut self,
        request: &StreamRequest,
        callback: Box<dyn AudioCallback>,
    ) -> Result<(), (DeviceError, Option<Box<dyn AudioCallback>>)> {
        #[cfg(feature = "midi")]
        let Some(mut midi) = self.midi_receiver.take() else {
            return Err((DeviceError::NoCallback, Some(callback)));
        };
        #[cfg(feature = "midi")]
        midi.restart();
        let mut parked = Some(Parked {
            callback,
            #[cfg(feature = "midi")]
            midi,
        });
        let returns = self.returns.0.clone();
        let max_block_size = self.max_block_size;
        let mut prepared = None;
        let result = self.backend.start(
            request,
            &mut |format: OpenedFormat| -> EngineCallback {
                let mut parked = parked.take();
                let format = StreamFormat {
                    sample_rate: f64::from(format.sample_rate),
                    max_block_size,
                    input_channels: format.input_channels,
                    output_channels: format.output_channels,
                };
                if let Some(parked) = &mut parked {
                    parked.callback.prepare(&format);
                }
                prepared = Some(format);
                let mut engine = Engine {
                    parked,
                    returns: returns.clone(),
                };
                Box::new(move |input, output, info| engine.run(input, output, info))
            },
            Arc::clone(&self.errors),
        );

        match result {
            Ok(started) => {
                let format = prepared.unwrap_or(StreamFormat {
                    sample_rate: f64::from(started.format.sample_rate),
                    max_block_size,
                    input_channels: started.format.input_channels,
                    output_channels: started.format.output_channels,
                });
                self.register(started, format, None);
                Ok(())
            }
            Err(error) => {
                // Either the backend failed before making the callback, and
                // it is still here, or it dropped what it made, which sent
                // the callback back.
                let parked = parked
                    .take()
                    .or_else(|| self.returns.1.recv_timeout(RETURN_TIMEOUT).ok());
                match parked {
                    Some(parked) => {
                        #[cfg(feature = "midi")]
                        {
                            self.midi_receiver = Some(parked.midi);
                        }
                        Err((error, Some(parked.callback)))
                    }
                    None => {
                        #[cfg(feature = "midi")]
                        self.renew_midi();
                        Err((error, None))
                    }
                }
            }
        }
    }
}

/// Running a callback at a fixed [`EngineFormat`], and changing devices
/// under it.
impl DeviceManager {
    fn open_engine(&mut self) -> Result<RestoreReport, DeviceError> {
        self.finish_crossfades(true);
        self.open_engine_after(None)
    }

    /// Opens the setup for the engine. `faded` is a stream that has
    /// already faded out and stopped, and when it did.
    fn open_engine_after(
        &mut self,
        mut faded: Option<(Arc<EdgeShared>, Instant)>,
    ) -> Result<RestoreReport, DeviceError> {
        let mut report = RestoreReport::default();
        let mut planned = self.plan_open(&mut report)?;
        // A backend that loads one driver at a time cannot describe another
        // while a stream runs on one, and plans the device it cannot
        // describe away. It could not have run beside the old stream anyway:
        // the old stream fades out and stops, and the setup is planned
        // again.
        let undescribed = planned.fallbacks.iter().any(|fallback| {
            matches!(
                fallback,
                Fallback::NoDevice {
                    wanted: Some(_),
                    ..
                }
            )
        });
        if undescribed && self.running.is_some() && !self.backend.concurrent_streams() {
            faded = self.fade_out_running().map(|old| (old, Instant::now()));
            self.collect_core();
            report = RestoreReport::default();
            planned = self.plan_open(&mut report)?;
        }
        let Some(engine) = &self.engine else {
            return Err(DeviceError::NoCallback);
        };
        let mut request = Self::request(
            &planned,
            engine.format.max_block_size,
            self.drift_correction,
        );
        if request.input.is_none() && request.output.is_none() {
            self.collect_core();
            return Ok(report);
        }
        match self.swap(&request, faded.clone()) {
            Ok(()) => Ok(report),
            Err(error) => {
                // An input that will not open must not keep the output
                // silent: try again without it.
                let (Some(_), Some(_), Some((input, _))) =
                    (&request.input, &request.output, &planned.input)
                else {
                    return Err(error);
                };
                request.input = None;
                self.swap(&request, faded)?;
                report.fallbacks.push(Fallback::InputDropped {
                    device: input.clone(),
                    reason: error.to_string(),
                });
                Ok(report)
            }
        }
    }

    /// Starts `request` in place of the running stream and hands it the
    /// engine: beside the old stream, crossfading, when the backend allows
    /// and the output device changes; otherwise after the old stream has
    /// faded out and stopped. `faded` is an old stream that has already
    /// faded out and stopped, and when it did.
    fn swap(
        &mut self,
        request: &StreamRequest,
        faded: Option<(Arc<EdgeShared>, Instant)>,
    ) -> Result<(), DeviceError> {
        let Some(engine) = &mut self.engine else {
            return Err(DeviceError::NoCallback);
        };
        let dead =
            std::mem::take(&mut engine.dead) || self.running.as_ref().is_some_and(Running::is_lost);
        let crossfade = !dead
            && faded.is_none()
            && engine.format.output_channels > 0
            && self.backend.concurrent_streams()
            && match (&self.running, &request.output) {
                (Some(running), Some(next)) => {
                    running.edge.is_some()
                        && running
                            .output
                            .as_ref()
                            .is_some_and(|now| now.device.id != next.id)
                }
                _ => false,
            };
        // A new device that will not open beside the old one may still
        // open in its place.
        if crossfade && self.crossfade(request).is_ok() {
            return Ok(());
        }

        let Some(engine) = &self.engine else {
            return Err(DeviceError::NoCallback);
        };
        let fade = engine.fade;
        let fade_in = engine.started;
        // What takes long to make is made while the old device still plays.
        let parts = Self::parts(request, &engine.format);
        let (kind, old, began) = match faded {
            Some((old, began)) => (SwapKind::Gap, Some(old), began),
            None => {
                let (kind, old) = match (&self.running, dead) {
                    (None, _) => (SwapKind::Start, None),
                    (Some(running), true) => (
                        SwapKind::Recovered,
                        running.edge.as_ref().map(|link| Arc::clone(&link.shared)),
                    ),
                    (Some(_), false) => (SwapKind::Gap, self.fade_out_running()),
                };
                (kind, old, Instant::now())
            }
        };

        self.collect_core();
        let core = self.engine.as_mut().and_then(|engine| engine.core.take());
        let Some(core) = core else {
            return Err(DeviceError::NoCallback);
        };
        let new = self.launch(request, Some(core), None, fade_in, parts)?;
        if let Some(engine) = &mut self.engine {
            engine.started = true;
            engine.last_swap = Some(SwapRecord {
                kind,
                fade,
                reopen: began.elapsed(),
                old,
                new,
            });
        }
        Ok(())
    }

    /// Fades the running stream out, and waits until its device has gone
    /// silent, or until it plainly is not calling. The stream still runs,
    /// silent, holding the engine. Returns what its edge shares.
    fn fade_out_running(&mut self) -> Option<Arc<EdgeShared>> {
        let fade = self.engine.as_ref()?.fade;
        let link = self.running.as_mut()?.edge.as_mut()?;
        let shared = Arc::clone(&link.shared);
        let retire = Retire {
            fade,
            successor: None,
        };
        if link.commands.push(retire).is_ok() {
            // The old device fades out on its own thread.
            let deadline = Instant::now() + fade + FADE_TIMEOUT;
            while let Some(running) = &self.running {
                if running.is_finished() || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Some(shared)
    }

    /// Starts `request` beside the running stream, which goes on running
    /// the engine, copies what it renders to the new one until the new
    /// one plays, and then fades out and hands the engine over.
    fn crossfade(&mut self, request: &StreamRequest) -> Result<(), DeviceError> {
        let (Some(engine), Some(running)) = (&self.engine, &self.running) else {
            return Err(DeviceError::NoCallback);
        };
        let Some(link) = &running.edge else {
            return Err(DeviceError::NoCallback);
        };
        let format = engine.format;
        let fade = engine.fade;
        // A quarter of a second, and never less than sixteen blocks: the
        // new edge keeps a block of each device of it, and lets go of the
        // rest when it starts.
        let frames = (format.sample_rate as usize / 4).max(16 * format.max_block_size);
        let (ring, ring_out) = spsc::channel::<f32>(frames * format.output_channels);
        let (handoff, handoff_out) = spsc::channel::<Box<Core>>(1);
        let incoming = Incoming {
            ring: ring_out,
            handoff: handoff_out,
            predecessor: Arc::clone(&link.shared),
            predecessor_rate: link.rate,
        };
        let old = self.running.take();
        let parts = Self::parts(request, &format);
        let new = match self.launch(request, None, Some(incoming), true, parts) {
            Ok(new) => new,
            Err(error) => {
                self.running = old;
                return Err(error);
            }
        };
        let Some(mut old) = old else {
            return Ok(());
        };
        let old_shared = old.edge.as_ref().map(|link| Arc::clone(&link.shared));
        if let Some(link) = &mut old.edge {
            let retire = Retire {
                fade,
                successor: Some(Successor {
                    ring: Some(ring),
                    handoff,
                    shared: Arc::clone(&new),
                }),
            };
            let _ = link.commands.push(retire);
        }
        if let Some(engine) = &mut self.engine {
            engine.retiring.push(old);
            engine.started = true;
            engine.last_swap = Some(SwapRecord {
                kind: SwapKind::Crossfade,
                fade,
                reopen: Duration::ZERO,
                old: old_shared,
                new,
            });
        }
        Ok(())
    }

    /// An edge's resamplers for the rate `request` asks of its device,
    /// when it asks for one.
    fn parts(request: &StreamRequest, format: &EngineFormat) -> Option<EdgeParts> {
        let rate = request
            .output
            .as_ref()
            .or(request.input.as_ref())
            .and_then(|endpoint| endpoint.settings.sample_rate)?;
        Some(EdgeParts::new(format, rate))
    }

    /// Starts `request` with a new edge, which runs `core` or waits for it
    /// from `incoming`, and makes it the running stream. On failure the
    /// core is back with the manager, unless the backend lost it.
    fn launch(
        &mut self,
        request: &StreamRequest,
        core: Option<Box<Core>>,
        incoming: Option<Incoming>,
        fade_in: bool,
        parts: Option<EdgeParts>,
    ) -> Result<Arc<EdgeShared>, DeviceError> {
        let Some(engine) = &self.engine else {
            return Err(DeviceError::NoCallback);
        };
        let format = engine.format;
        let had_core = core.is_some();
        let shared = EdgeShared::new();
        let (commands, commands_out) = spsc::channel::<Retire>(2);
        let mut init = Some(EdgeInit {
            engine: format,
            fade: engine.fade,
            fade_in,
            core,
            incoming,
            shared: Arc::clone(&shared),
            commands: commands_out,
            returns: engine.returns.0.clone(),
            parts,
        });
        let mut opened = None;
        let result = self.backend.start(
            request,
            &mut |device: OpenedFormat| -> EngineCallback {
                opened = Some(device);
                let mut edge = init.take().map(|init| Edge::new(init, device));
                Box::new(move |input, output, info| {
                    if let Some(edge) = &mut edge {
                        edge.run(input, output, info);
                    }
                })
            },
            Arc::clone(&self.errors),
        );
        match result {
            Ok(started) => {
                let device = opened.unwrap_or(started.format);
                let rate = match device.sample_rate {
                    0 => format.sample_rate,
                    rate => rate,
                };
                let stream_format = StreamFormat {
                    sample_rate: f64::from(rate),
                    max_block_size: format.max_block_size,
                    input_channels: device.input_channels,
                    output_channels: device.output_channels,
                };
                let link = EdgeLink {
                    shared: Arc::clone(&shared),
                    commands,
                    rate,
                };
                self.register(started, stream_format, Some(link));
                Ok(shared)
            }
            Err(error) => {
                // Either the backend failed before making the edge, and
                // the core is still here, or it dropped the edge, which
                // sent the core back.
                let core = match init.take() {
                    Some(init) => init.core,
                    None if had_core => self
                        .engine
                        .as_ref()
                        .and_then(|engine| engine.returns.1.recv_timeout(RETURN_TIMEOUT).ok()),
                    None => None,
                };
                match (core, &mut self.engine) {
                    (Some(core), Some(engine)) => engine.core = Some(core),
                    (None, _) if had_core => {
                        // Lost with the edge, and the MIDI queue with it.
                        #[cfg(feature = "midi")]
                        self.renew_midi();
                    }
                    _ => {}
                }
                Err(error)
            }
        }
    }

    /// Stops every stream and brings the engine back to the manager.
    fn collect_core(&mut self) {
        let Some(engine) = &mut self.engine else {
            return;
        };
        let retiring = std::mem::take(&mut engine.retiring);
        let running = self.running.take();
        if running.is_some() {
            if let Ok(mut watched) = self.watched.lock() {
                *watched = None;
            }
        }
        let any = !retiring.is_empty() || running.is_some();
        // Oldest first: each hands the engine on to the stream after it,
        // and the last one to the manager.
        drop(retiring);
        drop(running);
        let lost = any
            && engine.core.is_none()
            && match engine.returns.1.recv_timeout(RETURN_TIMEOUT) {
                Ok(core) => {
                    engine.core = Some(core);
                    false
                }
                Err(_) => true,
            };
        if lost {
            // The backend never dropped the edge: the engine is gone, and
            // the MIDI queue with it.
            #[cfg(feature = "midi")]
            self.renew_midi();
        }
    }

    /// Closes the old streams of crossfades that have faded out. With
    /// `wait`, first waits for those still fading, a little longer than
    /// the fade, and then closes them all.
    fn finish_crossfades(&mut self, wait: bool) {
        let Some(engine) = &mut self.engine else {
            return;
        };
        if wait {
            let deadline = Instant::now() + engine.fade + FADE_TIMEOUT;
            while engine.retiring.iter().any(|old| !old.is_finished()) && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            engine.retiring.clear();
        } else {
            engine.retiring.retain(|old| !old.is_finished());
        }
    }

    /// Leaves the engine format: stops the streams, and gives the callback
    /// back, released.
    fn leave_engine(&mut self) -> Option<Box<dyn AudioCallback>> {
        self.engine.as_ref()?;
        self.collect_core();
        let core = self.engine.take()?.core?;
        let Parked {
            mut callback,
            #[cfg(feature = "midi")]
            midi,
        } = core.parked;
        callback.release();
        #[cfg(feature = "midi")]
        {
            self.midi_receiver = Some(midi);
        }
        Some(callback)
    }
}

#[cfg(feature = "midi")]
impl DeviceManager {
    /// Opens the MIDI ports `planned` names and closes those it does not.
    fn connect_midi(&mut self, planned: &Plan, report: &mut RestoreReport) {
        self.midi_inputs
            .retain(|(port, _)| planned.midi_inputs.iter().any(|other| other.id == port.id));
        for port in &planned.midi_inputs {
            if self.midi_inputs.iter().any(|(open, _)| open.id == port.id) {
                continue;
            }
            let sink = MidiSink::Shared(Arc::clone(&self.midi_shared));
            match self.backend.connect_midi_input(&port.id, sink) {
                Ok(connection) => self.midi_inputs.push((port.clone(), connection)),
                Err(error) => report.fallbacks.push(Fallback::MidiPortMissing {
                    direction: Direction::Input,
                    wanted: DeviceRef::from(port),
                    reason: error.to_string(),
                }),
            }
        }
        match &planned.midi_output {
            None => self.replace_midi_output(None),
            Some(port) => {
                if self.midi_output.as_ref().map(|open| &open.id) != Some(&port.id) {
                    match self.backend.connect_midi_output(&port.id) {
                        Ok(sender) => self.replace_midi_output(Some((port.clone(), sender))),
                        Err(error) => {
                            self.replace_midi_output(None);
                            report.fallbacks.push(Fallback::MidiPortMissing {
                                direction: Direction::Output,
                                wanted: DeviceRef::from(port),
                                reason: error.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    /// Puts `next` in place of the open MIDI output port, after ending
    /// the notes the old one plays.
    fn replace_midi_output(&mut self, next: Option<(DeviceSummary, Box<dyn MidiSender>)>) {
        self.midi_output = next.as_ref().map(|(port, _)| port.clone());
        SharedOutput::lock(&self.midi_shared_output).replace(next.map(|(_, sender)| sender));
    }

    fn open_midi_ports(&self) -> Vec<String> {
        self.midi_inputs
            .iter()
            .map(|(port, _)| port.id.clone())
            .chain(self.midi_output.iter().map(|port| port.id.clone()))
            .collect()
    }

    /// A new MIDI queue for the callback, when the old receiver was lost
    /// with a callback that never came back. The ports are connected to it
    /// again.
    fn renew_midi(&mut self) {
        let (shared, receiver) = merged_receiver();
        self.midi_shared = shared;
        self.midi_receiver = Some(receiver);
        let ports: Vec<DeviceSummary> = self.midi_inputs.drain(..).map(|(port, _)| port).collect();
        for port in ports {
            let sink = MidiSink::Shared(Arc::clone(&self.midi_shared));
            if let Ok(connection) = self.backend.connect_midi_input(&port.id, sink) {
                self.midi_inputs.push((port, connection));
            }
        }
    }

    /// The MIDI input ports that are open.
    pub fn midi_inputs(&self) -> Vec<DeviceSummary> {
        self.midi_inputs
            .iter()
            .map(|(port, _)| port.clone())
            .collect()
    }

    /// The MIDI output port that is open.
    pub fn midi_output(&self) -> Option<&DeviceSummary> {
        self.midi_output.as_ref()
    }

    /// The protocol the open MIDI output port speaks; every port the
    /// manager opens speaks MIDI 1.0 for now.
    pub fn midi_output_protocol(&self) -> Option<MidiProtocol> {
        SharedOutput::lock(&self.midi_shared_output).protocol()
    }

    /// Makes a queue from the audio thread to the MIDI output port, and
    /// starts the thread that sends what it carries, each packet when it
    /// is due (see [`MidiTransmitter`] for the timing). The callback keeps
    /// the transmitter, calls [`MidiTransmitter::begin_block`] at the start
    /// of every block and [`MidiTransmitter::send`] for each packet; both
    /// are real-time safe. Room for `capacity` packets waiting.
    ///
    /// The packets go to whichever port is open when they are due, so the
    /// port can change ([`set_midi_output`](Self::set_midi_output),
    /// [`apply`](Self::apply), [`follow`](Self::follow)) while the audio
    /// thread sends, and the audio thread never knows: it only pushes into
    /// the queue, which neither allocates, locks nor waits. A packet due
    /// while no port is open is dropped.
    ///
    /// A port that is changed or closed is first sent a note-off for every
    /// note it plays, and a controller 64 of zero for every sustain pedal
    /// it holds down, so that none is left hanging there, and a note-off
    /// for a note the open port is not playing, such as one a change of
    /// port has already ended, is not sent. So every note-on reaches one
    /// port, and that port receives one note-off for it.
    ///
    /// Calling it again replaces the queue: the old one's thread sends
    /// what is waiting and stops. Allocates and starts a thread.
    pub fn midi_output_transmitter(&mut self, capacity: usize) -> MidiTransmitter {
        self.midi_sending = None;
        let (transmitter, queue) = midi_output_queue(capacity);
        self.midi_sending = Some(SharedOutputThread::start(
            Arc::clone(&self.midi_shared_output),
            queue,
        ));
        transmitter
    }

    /// Whether the thread that sends the audio thread's packets runs: it
    /// does from [`midi_output_transmitter`](Self::midi_output_transmitter)
    /// on, unless the system refused to start it.
    pub fn is_sending_midi(&self) -> bool {
        self.midi_sending
            .as_ref()
            .is_some_and(SharedOutputThread::is_running)
    }

    /// How many messages queued through
    /// [`midi_output_transmitter`](Self::midi_output_transmitter) the
    /// ports refused.
    pub fn midi_output_errors(&self) -> u64 {
        SharedOutput::lock(&self.midi_shared_output).errors()
    }

    /// How many notes were ended by a change or a close of the MIDI output
    /// port, each with a note-off to the port that played it.
    pub fn midi_output_notes_ended(&self) -> u64 {
        SharedOutput::lock(&self.midi_shared_output).released()
    }

    /// Adds a MIDI input port to the setup and opens it at once, without
    /// restarting the audio.
    pub fn enable_midi_input(&mut self, port: DeviceRef) -> Result<(), MidiError> {
        if !self.setup.midi_inputs.contains(&port) {
            self.setup.midi_inputs.push(port.clone());
        }
        if self.midi_inputs.iter().any(|(open, _)| open.id == port.id) {
            return Ok(());
        }
        let sink = MidiSink::Shared(Arc::clone(&self.midi_shared));
        let connection = self.backend.connect_midi_input(&port.id, sink)?;
        self.midi_inputs.push((
            DeviceSummary {
                id: port.id,
                name: port.name,
            },
            connection,
        ));
        Ok(())
    }

    /// Takes a MIDI input port out of the setup and closes it.
    pub fn disable_midi_input(&mut self, id: &str) {
        self.setup.midi_inputs.retain(|port| port.id != id);
        self.midi_inputs.retain(|(port, _)| port.id != id);
    }

    /// Sets the MIDI output port of the setup and opens it, or closes it
    /// with `None`, without touching the audio. The port open before is
    /// first sent a note-off for every note it plays and its sustain
    /// pedals are lifted; the same port again
    /// is left open as it is.
    ///
    /// A port that cannot be opened returns the error and leaves no port
    /// open, but stays in the setup, so that [`follow`](Self::follow) opens
    /// it when it appears.
    pub fn set_midi_output(&mut self, port: Option<DeviceRef>) -> Result<(), MidiError> {
        self.setup.midi_output = port.clone();
        let Some(port) = port else {
            self.replace_midi_output(None);
            return Ok(());
        };
        if self
            .midi_output
            .as_ref()
            .is_some_and(|open| open.id == port.id)
        {
            return Ok(());
        }
        match self.backend.connect_midi_output(&port.id) {
            Ok(sender) => {
                let summary = DeviceSummary {
                    id: port.id,
                    name: port.name,
                };
                self.replace_midi_output(Some((summary, sender)));
                Ok(())
            }
            Err(error) => {
                self.replace_midi_output(None);
                Err(error)
            }
        }
    }

    /// Sends one complete MIDI message to the open MIDI output port.
    ///
    /// This talks to the operating system and may block: never call it
    /// from the audio thread.
    pub fn send_midi(&mut self, bytes: &[u8]) -> Result<(), MidiError> {
        SharedOutput::lock(&self.midi_shared_output).send_bytes(bytes)
    }

    /// Sends one Universal MIDI Packet to the open MIDI output port: as
    /// it is to a port that speaks MIDI 2.0, as the MIDI 1.0 bytes it
    /// stands for to the others; see
    /// [`PacketWriter`](crate::midi::PacketWriter) for what becomes what.
    ///
    /// This talks to the operating system and may block: never call it
    /// from the audio thread; there, use
    /// [`midi_output_transmitter`](Self::midi_output_transmitter).
    pub fn send_midi_packet(
        &mut self,
        packet: &manguebyte_midi::ump::Packet,
    ) -> Result<(), MidiError> {
        SharedOutput::lock(&self.midi_shared_output).send_packet(packet)
    }
}

impl Drop for DeviceManager {
    fn drop(&mut self) {
        // The stream goes before the MIDI ports that feed it.
        self.stop();
        drop(self.leave_engine());
        // Then what the audio thread queued for the MIDI output, and the
        // port, which ends the notes it plays.
        #[cfg(feature = "midi")]
        {
            self.midi_sending = None;
            self.replace_midi_output(None);
        }
    }
}
