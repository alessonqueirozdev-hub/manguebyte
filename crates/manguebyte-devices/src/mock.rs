//! A stand-in for the system's audio and MIDI, with no hardware behind it.

use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use cpal::{SampleFormat, SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange};
use manguebyte_core::AudioBuffer;
#[cfg(feature = "midi")]
use manguebyte_midi::StreamParser;

#[cfg(feature = "midi")]
use crate::backend::MidiSender;
use crate::backend::{
    Backend, Endpoint, EngineCallback, ErrorSink, OpenedEndpoint, OpenedFormat, Started,
    StreamRequest,
};
use crate::blocks::{BlockCapturer, BlockClock, BlockRenderer};
use crate::duplex::{Feeder, Renderer, Stats, bridge, monotonic_ns};
#[cfg(feature = "midi")]
use crate::midi::{MidiError, MidiSink};
use crate::settings::resolve;
use crate::timing::{StreamHealth, frame_duration};
use crate::{
    CallbackInfo, ConfigRange, DeviceError, DeviceInfo, DeviceSnapshot, DeviceSummary, Direction,
    HostInfo, StreamSettings,
};

type OutputCallback = Box<dyn FnMut(&mut AudioBuffer<f32>, &CallbackInfo) + Send>;
type InputCallback = Box<dyn FnMut(&AudioBuffer<f32>, &CallbackInfo) + Send>;

enum MockKind {
    Output(BlockRenderer<OutputCallback>),
    Input(BlockCapturer<InputCallback>),
    Duplex(Box<(Feeder, Renderer<EngineCallback>)>),
}

struct MockStream {
    token: u64,
    kind: MockKind,
    errors: ErrorSink,
    sample_rate: f64,
    input: Option<(String, StreamSettings, Arc<StreamHealth>)>,
    output: Option<(String, StreamSettings, Arc<StreamHealth>)>,
    input_frames: u64,
    output_frames: u64,
}

#[cfg(feature = "midi")]
struct MockMidiConnection {
    token: u64,
    port: String,
    sink: MidiSink,
    parser: StreamParser,
}

struct MockState {
    hosts: Vec<String>,
    host: usize,
    devices: Vec<(Direction, DeviceInfo)>,
    default_output: Option<String>,
    default_input: Option<String>,
    midi_inputs: Vec<DeviceSummary>,
    midi_outputs: Vec<DeviceSummary>,
    listing_fails: bool,
    input_latency: Option<Duration>,
    output_latency: Option<Duration>,
    /// Running streams, oldest first.
    streams: Vec<MockStream>,
    /// Whether a second stream may start while one runs.
    concurrent: bool,
    next_token: u64,
    #[cfg(feature = "midi")]
    midi_connections: Vec<MockMidiConnection>,
    sent_midi: Vec<(String, Vec<u8>)>,
    /// Whether sending to a MIDI output port waits, as a hung driver does.
    #[cfg(feature = "midi")]
    midi_output_held: bool,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            hosts: vec![String::from("Mock")],
            host: 0,
            devices: Vec::new(),
            default_output: None,
            default_input: None,
            midi_inputs: Vec::new(),
            midi_outputs: Vec::new(),
            listing_fails: false,
            input_latency: None,
            output_latency: None,
            streams: Vec::new(),
            concurrent: false,
            next_token: 0,
            #[cfg(feature = "midi")]
            midi_connections: Vec::new(),
            sent_midi: Vec::new(),
            #[cfg(feature = "midi")]
            midi_output_held: false,
        }
    }
}

/// A stand-in for the system's audio and MIDI devices, driven by hand.
///
/// A [`DeviceManager`](crate::DeviceManager) made with
/// [`DeviceManager::headless`](crate::DeviceManager::headless) finds its
/// devices here, and a [`DeviceWatcher`](crate::DeviceWatcher)-style poll
/// sees them come and go as the test adds and removes them. Nothing runs
/// by itself: a test calls [`render`](Self::render) and
/// [`capture`](Self::capture) where a sound card would call the stream, so
/// everything is deterministic and works on any machine, with no sound
/// card.
///
/// Clones share the same devices.
///
/// ```
/// use manguebyte_devices::{DeviceManager, DeviceSetup, Direction, MockBackend};
///
/// let mock = MockBackend::new();
/// mock.add_device(Direction::Output, MockBackend::device("out", "Speakers", 2, 48_000));
///
/// let mut manager = DeviceManager::headless(&mock);
/// let report = manager.start(&DeviceSetup::default(), |io: &mut manguebyte_devices::DeviceIo<'_>| {
///     io.output.channel_mut(0).fill(0.5);
/// })?;
/// assert!(report.is_exact());
///
/// let mut played = [0.0f32; 2 * 64];
/// assert!(mock.render(&mut played));
/// assert_eq!(played[0], 0.5);
/// # Ok::<(), manguebyte_devices::DeviceError>(())
/// ```
#[derive(Clone, Default)]
pub struct MockBackend {
    state: Arc<Mutex<MockState>>,
}

impl std::fmt::Debug for MockBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockBackend").finish_non_exhaustive()
    }
}

fn lock(state: &Mutex<MockState>) -> MutexGuard<'_, MockState> {
    match state.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl MockBackend {
    /// A system with no devices, one host called `"Mock"`, and no MIDI
    /// ports.
    pub fn new() -> Self {
        Self::default()
    }

    /// A device with `channels` channels at most, that runs at any rate
    /// from 8 kHz to 192 kHz with buffers of 16 to 4,096 frames, and
    /// prefers `sample_rate` and all its channels.
    pub fn device(id: &str, name: &str, channels: u16, sample_rate: u32) -> DeviceInfo {
        DeviceInfo {
            id: id.to_owned(),
            name: name.to_owned(),
            is_default: false,
            default_sample_rate: Some(sample_rate),
            default_channels: Some(channels),
            configs: (1..=channels)
                .map(|channels| ConfigRange {
                    channels,
                    min_sample_rate: 8_000,
                    max_sample_rate: 192_000,
                    buffer_size: Some((16, 4_096)),
                })
                .collect(),
        }
    }

    fn state(&self) -> MutexGuard<'_, MockState> {
        lock(&self.state)
    }

    /// Connects a device. The first one of each direction becomes the
    /// default.
    pub fn add_device(&self, direction: Direction, device: DeviceInfo) {
        let mut state = self.state();
        let default = match direction {
            Direction::Output => &mut state.default_output,
            Direction::Input => &mut state.default_input,
        };
        if default.is_none() {
            *default = Some(device.id.clone());
        }
        state
            .devices
            .retain(|(other, info)| *other != direction || info.id != device.id);
        state.devices.push((direction, device));
    }

    /// Disconnects a device. A stream running on it stops being called,
    /// as a real one does when its device is unplugged. Returns whether it
    /// was connected.
    pub fn remove_device(&self, direction: Direction, id: &str) -> bool {
        let mut state = self.state();
        let before = state.devices.len();
        state
            .devices
            .retain(|(other, info)| *other != direction || info.id != id);
        let default = match direction {
            Direction::Output => &mut state.default_output,
            Direction::Input => &mut state.default_input,
        };
        if default.as_deref() == Some(id) {
            *default = None;
        }
        state.devices.len() != before
    }

    /// Makes a device the default, or leaves the direction with none.
    pub fn set_default_device(&self, direction: Direction, id: Option<&str>) {
        let mut state = self.state();
        let default = match direction {
            Direction::Output => &mut state.default_output,
            Direction::Input => &mut state.default_input,
        };
        *default = id.map(str::to_owned);
    }

    /// Adds another host that can be selected by name.
    pub fn add_host(&self, name: &str) {
        let mut state = self.state();
        if !state.hosts.iter().any(|host| host == name) {
            state.hosts.push(name.to_owned());
        }
    }

    /// Connects a MIDI port.
    pub fn add_midi_port(&self, direction: Direction, id: &str, name: &str) {
        let mut state = self.state();
        let ports = match direction {
            Direction::Input => &mut state.midi_inputs,
            Direction::Output => &mut state.midi_outputs,
        };
        ports.retain(|port| port.id != id);
        ports.push(DeviceSummary {
            id: id.to_owned(),
            name: name.to_owned(),
        });
    }

    /// Disconnects a MIDI port.
    pub fn remove_midi_port(&self, direction: Direction, id: &str) {
        let mut state = self.state();
        match direction {
            Direction::Input => state.midi_inputs.retain(|port| port.id != id),
            Direction::Output => state.midi_outputs.retain(|port| port.id != id),
        }
    }

    /// Makes every message sent to a MIDI output port wait, as a driver
    /// that hangs does, until called again with `false`. Rendering and
    /// capturing go on meanwhile.
    #[cfg(feature = "midi")]
    pub fn hold_midi_output(&self, held: bool) {
        self.state().midi_output_held = held;
    }

    /// Makes listing the devices fail, as a backend whose service went away
    /// does, until called again with `false`.
    pub fn fail_listing(&self, fail: bool) {
        self.state().listing_fails = fail;
    }

    /// The latencies the mock reports to callbacks.
    pub fn set_latency(&self, input: Option<Duration>, output: Option<Duration>) {
        let mut state = self.state();
        state.input_latency = input;
        state.output_latency = output;
    }

    /// Lets a second stream start while one runs, as most systems do
    /// (`true`), or refuses it, as a backend that loads one driver at a
    /// time does (`false`, the default). With several streams running,
    /// [`render_on`](Self::render_on) and [`capture_on`](Self::capture_on)
    /// say which device calls.
    pub fn set_concurrent_streams(&self, on: bool) {
        self.state().concurrent = on;
    }

    /// Whether a stream is running.
    pub fn is_running(&self) -> bool {
        !self.state().streams.is_empty()
    }

    /// How many streams are running.
    pub fn stream_count(&self) -> usize {
        self.state().streams.len()
    }

    /// The device and settings the newest stream with that side has in one
    /// direction.
    pub fn running(&self, direction: Direction) -> Option<(String, StreamSettings)> {
        let state = self.state();
        state.streams.iter().rev().find_map(|stream| {
            stream
                .side(direction)
                .map(|(id, settings, _)| (id.clone(), *settings))
        })
    }

    fn connected(state: &MockState, direction: Direction, id: &str) -> bool {
        state
            .devices
            .iter()
            .any(|(other, info)| *other == direction && info.id == id)
    }

    /// Asks the running stream for interleaved output, as the output
    /// device would. Returns `false`, leaving `output` alone, when no
    /// stream plays, or when its output device has been removed. With
    /// several streams running, the newest that plays is asked.
    ///
    /// Allocates nothing, so a test can count allocations around it.
    pub fn render(&self, output: &mut [f32]) -> bool {
        self.render_where(output, None)
    }

    /// Asks the stream that plays to the output device `id` for
    /// interleaved output, as that device would. Returns `false`, leaving
    /// `output` alone, when no stream plays there, or when the device has
    /// been removed.
    ///
    /// Allocates nothing.
    pub fn render_on(&self, id: &str, output: &mut [f32]) -> bool {
        self.render_where(output, Some(id))
    }

    /// The newest stream with a side in `direction`, on `device` if given,
    /// whose device there is connected.
    fn find(state: &MockState, direction: Direction, device: Option<&str>) -> Option<usize> {
        let index = state.streams.iter().rposition(|stream| {
            stream
                .side(direction)
                .is_some_and(|(id, _, _)| device.is_none_or(|device| device == id))
        })?;
        let (id, _, _) = state.streams[index].side(direction)?;
        Self::connected(state, direction, id).then_some(index)
    }

    fn render_where(&self, output: &mut [f32], device: Option<&str>) -> bool {
        let mut state = self.state();
        let latency = state.output_latency;
        let Some(index) = Self::find(&state, Direction::Output, device) else {
            return false;
        };
        let stream = &mut state.streams[index];
        let channels = stream
            .output
            .as_ref()
            .and_then(|(_, settings, _)| settings.channels)
            .map_or(1, usize::from)
            .max(1);
        let time = frame_duration(stream.output_frames as usize, stream.sample_rate);
        match &mut stream.kind {
            MockKind::Output(renderer) => renderer.render(output, time, latency),
            MockKind::Duplex(duplex) => {
                duplex.1.render(output, time, latency, monotonic_ns());
            }
            MockKind::Input(_) => return false,
        }
        stream.output_frames += (output.len() / channels) as u64;
        true
    }

    /// Hands the running stream interleaved captured audio, as the input
    /// device would. Returns `false` when no stream records, or when its
    /// input device has been removed. With several streams running, the
    /// newest that records is handed it.
    ///
    /// Allocates nothing.
    pub fn capture(&self, input: &[f32]) -> bool {
        self.capture_where(input, None)
    }

    /// Hands the stream that records from the input device `id`
    /// interleaved captured audio, as that device would. Returns `false`
    /// when no stream records there, or when the device has been removed.
    ///
    /// Allocates nothing.
    pub fn capture_on(&self, id: &str, input: &[f32]) -> bool {
        self.capture_where(input, Some(id))
    }

    fn capture_where(&self, input: &[f32], device: Option<&str>) -> bool {
        let mut state = self.state();
        let latency = state.input_latency;
        let Some(index) = Self::find(&state, Direction::Input, device) else {
            return false;
        };
        let stream = &mut state.streams[index];
        let channels = stream
            .input
            .as_ref()
            .and_then(|(_, settings, _)| settings.channels)
            .map_or(1, usize::from)
            .max(1);
        let time = frame_duration(stream.input_frames as usize, stream.sample_rate);
        match &mut stream.kind {
            MockKind::Input(capturer) => capturer.capture(input, time, latency),
            MockKind::Duplex(duplex) => duplex.0.feed(input, latency, monotonic_ns()),
            MockKind::Output(_) => return false,
        }
        stream.input_frames += (input.len() / channels) as u64;
        true
    }

    /// Makes the backend report that the running stream's device in
    /// `direction` went away, as a real one does from its error callback.
    /// Returns whether a stream was running there.
    pub fn report_device_lost(&self, direction: Direction) -> bool {
        let (errors, id) = {
            let state = self.state();
            let Some((stream, (id, _, health))) = state
                .streams
                .iter()
                .rev()
                .find_map(|stream| Some((stream, stream.side(direction)?)))
            else {
                return false;
            };
            health.mark_lost();
            (Arc::clone(&stream.errors), id.clone())
        };
        // Outside the lock: the handler may well call back into the mock.
        errors(DeviceError::Backend(cpal::Error::with_message(
            cpal::ErrorKind::DeviceNotAvailable,
            format!("the mock device {id} was unplugged"),
        )));
        true
    }

    /// Makes the newest stream with a side in `direction` ask to be opened
    /// again, as a driver does when its settings change under it (an ASIO
    /// reset request, for one). Returns whether a stream was running there.
    pub fn request_restart(&self, direction: Direction) -> bool {
        let state = self.state();
        let side = state
            .streams
            .iter()
            .rev()
            .find_map(|stream| stream.side(direction));
        match side {
            Some((_, _, health)) => {
                health.request_restart();
                true
            }
            None => false,
        }
    }

    /// Delivers raw MIDI bytes from an input port to whoever has it open.
    /// Returns whether anyone had.
    #[cfg(feature = "midi")]
    pub fn send_midi(&self, port: &str, bytes: &[u8]) -> bool {
        let mut state = self.state();
        let mut delivered = false;
        for connection in &mut state.midi_connections {
            if connection.port == port {
                connection.sink.deliver(bytes, &mut connection.parser);
                delivered = true;
            }
        }
        delivered
    }

    /// Takes what has been sent to MIDI output ports so far, with the
    /// identifier of each port.
    pub fn take_sent_midi(&self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut self.state().sent_midi)
    }

    fn open(
        state: &MockState,
        direction: Direction,
        endpoint: &Endpoint,
    ) -> Result<(DeviceSummary, StreamSettings), DeviceError> {
        let (_, info) = state
            .devices
            .iter()
            .find(|(other, info)| *other == direction && info.id == endpoint.id)
            .ok_or_else(|| DeviceError::DeviceNotFound(endpoint.id.clone()))?;
        let buffer = |range: Option<(u32, u32)>| match range {
            Some((min, max)) => SupportedBufferSize::Range { min, max },
            None => SupportedBufferSize::Unknown,
        };
        let ranges: Vec<SupportedStreamConfigRange> = info
            .configs
            .iter()
            .map(|config| {
                SupportedStreamConfigRange::new(
                    config.channels,
                    config.min_sample_rate,
                    config.max_sample_rate,
                    buffer(config.buffer_size),
                    SampleFormat::F32,
                )
            })
            .collect();
        let default = match (info.default_channels, info.default_sample_rate) {
            (Some(channels), Some(rate)) => Some(SupportedStreamConfig::new(
                channels,
                rate,
                SupportedBufferSize::Unknown,
                SampleFormat::F32,
            )),
            _ => None,
        };
        let resolved = resolve(&ranges, default, &endpoint.settings)?;
        let settings = StreamSettings {
            sample_rate: Some(resolved.config.sample_rate),
            channels: Some(resolved.config.channels),
            buffer_size: match resolved.config.buffer_size {
                cpal::BufferSize::Fixed(frames) => Some(frames),
                cpal::BufferSize::Default => None,
            },
        };
        Ok((
            DeviceSummary {
                id: info.id.clone(),
                name: info.name.clone(),
            },
            settings,
        ))
    }
}

/// Stops the mock's stream when the device manager drops it.
struct StreamGuard {
    state: Weak<Mutex<MockState>>,
    token: u64,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let Some(state) = self.state.upgrade() else {
            return;
        };
        let stream = {
            let mut state = lock(&state);
            let index = state
                .streams
                .iter()
                .position(|stream| stream.token == self.token);
            index.map(|index| state.streams.remove(index))
        };
        // The callback goes back to the manager as the stream is dropped,
        // outside the lock.
        drop(stream);
    }
}

#[cfg(feature = "midi")]
struct MidiGuard {
    state: Weak<Mutex<MockState>>,
    token: u64,
}

#[cfg(feature = "midi")]
impl Drop for MidiGuard {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            lock(&state)
                .midi_connections
                .retain(|connection| connection.token != self.token);
        }
    }
}

#[cfg(feature = "midi")]
struct MockMidiOutput {
    state: Weak<Mutex<MockState>>,
    port: String,
}

#[cfg(feature = "midi")]
impl MidiSender for MockMidiOutput {
    fn send(&mut self, bytes: &[u8]) -> Result<(), MidiError> {
        let shared = self
            .state
            .upgrade()
            .ok_or_else(|| MidiError::Send(String::from("the mock is gone")))?;
        let mut state = lock(&shared);
        // A held port waits, without keeping the mock from rendering.
        while state.midi_output_held {
            drop(state);
            std::thread::sleep(Duration::from_millis(1));
            state = lock(&shared);
        }
        if !state.midi_outputs.iter().any(|port| port.id == self.port) {
            return Err(MidiError::Send(format!("{} is not connected", self.port)));
        }
        let port = self.port.clone();
        state.sent_midi.push((port, bytes.to_vec()));
        Ok(())
    }
}

impl MockStream {
    fn side(&self, direction: Direction) -> Option<&(String, StreamSettings, Arc<StreamHealth>)> {
        match direction {
            Direction::Input => self.input.as_ref(),
            Direction::Output => self.output.as_ref(),
        }
    }
}

impl Backend for MockBackend {
    fn host_name(&self) -> String {
        let state = self.state();
        state.hosts[state.host].clone()
    }

    fn hosts(&self) -> Vec<HostInfo> {
        self.state()
            .hosts
            .iter()
            .enumerate()
            .map(|(index, name)| HostInfo {
                name: name.clone(),
                is_default: index == 0,
            })
            .collect()
    }

    fn select_host(&self, name: Option<&str>) -> Result<(), DeviceError> {
        let mut state = self.state();
        let index = match name {
            None => 0,
            Some(name) => state
                .hosts
                .iter()
                .position(|host| host.eq_ignore_ascii_case(name.trim()))
                .ok_or_else(|| DeviceError::HostUnavailable(name.to_owned()))?,
        };
        state.host = index;
        Ok(())
    }

    fn snapshot(&self) -> Result<DeviceSnapshot, DeviceError> {
        let state = self.state();
        if state.listing_fails {
            return Err(DeviceError::HostUnavailable(
                state.hosts[state.host].clone(),
            ));
        }
        let list = |direction: Direction| {
            state
                .devices
                .iter()
                .filter(|(other, _)| *other == direction)
                .map(|(_, info)| DeviceSummary {
                    id: info.id.clone(),
                    name: info.name.clone(),
                })
                .collect()
        };
        Ok(DeviceSnapshot {
            outputs: list(Direction::Output),
            inputs: list(Direction::Input),
            default_output: state.default_output.clone(),
            default_input: state.default_input.clone(),
            midi_inputs: state.midi_inputs.clone(),
            midi_outputs: state.midi_outputs.clone(),
        })
    }

    fn device_info(&self, direction: Direction, id: &str) -> Result<DeviceInfo, DeviceError> {
        let state = self.state();
        state
            .devices
            .iter()
            .find(|(other, info)| *other == direction && info.id == id)
            .map(|(_, info)| {
                let mut info = info.clone();
                info.is_default = match direction {
                    Direction::Output => state.default_output.as_deref() == Some(id),
                    Direction::Input => state.default_input.as_deref() == Some(id),
                };
                info
            })
            .ok_or_else(|| DeviceError::DeviceNotFound(id.to_owned()))
    }

    fn concurrent_streams(&self) -> bool {
        self.state().concurrent
    }

    fn start(
        &self,
        request: &StreamRequest,
        make: &mut dyn FnMut(OpenedFormat) -> EngineCallback,
        on_error: ErrorSink,
    ) -> Result<Started, DeviceError> {
        let state = self.state();
        if !state.concurrent && !state.streams.is_empty() {
            return Err(DeviceError::UnsupportedSettings(String::from(
                "the mock runs one stream at a time",
            )));
        }
        let input = request
            .input
            .as_ref()
            .map(|endpoint| Self::open(&state, Direction::Input, endpoint))
            .transpose()?;
        let output = request
            .output
            .as_ref()
            .map(|endpoint| Self::open(&state, Direction::Output, endpoint))
            .transpose()?;
        let rate = |side: &Option<(DeviceSummary, StreamSettings)>| {
            side.as_ref().and_then(|(_, settings)| settings.sample_rate)
        };
        if let (Some(input), Some(output)) = (rate(&input), rate(&output)) {
            if input != output {
                return Err(DeviceError::SampleRateMismatch { input, output });
            }
        }
        let channels = |side: &Option<(DeviceSummary, StreamSettings)>| {
            side.as_ref()
                .and_then(|(_, settings)| settings.channels)
                .map_or(0, usize::from)
        };
        let format = OpenedFormat {
            sample_rate: rate(&output).or(rate(&input)).unwrap_or(0),
            input_channels: channels(&input),
            output_channels: channels(&output),
        };
        if input.is_none() && output.is_none() {
            return Err(DeviceError::UnsupportedSettings(String::from(
                "neither an input nor an output was asked for",
            )));
        }

        // The callback is prepared outside the lock: it is the
        // application's code, and may well look at the mock.
        drop(state);
        let mut callback = make(format);
        let mut state = self.state();
        if !state.concurrent && !state.streams.is_empty() {
            return Err(DeviceError::UnsupportedSettings(String::from(
                "the mock runs one stream at a time",
            )));
        }

        let block = request.duplex.max_block_size.max(1);
        let sample_rate = f64::from(format.sample_rate);
        let input_health = StreamHealth::new();
        let output_health = StreamHealth::new();
        let kind = match (&input, &output) {
            (Some(_), Some(_)) => {
                let (feeder, renderer) = bridge(
                    format.input_channels,
                    format.output_channels,
                    format.sample_rate as usize,
                    request.duplex,
                    callback,
                    Arc::new(Stats::default()),
                    Arc::clone(&input_health),
                    Arc::clone(&output_health),
                );
                MockKind::Duplex(Box::new((feeder, renderer)))
            }
            (None, Some(_)) => {
                let mut nothing = AudioBuffer::new(0, block);
                MockKind::Output(BlockRenderer::new(
                    format.output_channels,
                    block,
                    BlockClock::new(sample_rate, Arc::clone(&output_health)),
                    Box::new(move |played: &mut AudioBuffer<f32>, info: &CallbackInfo| {
                        nothing.set_num_frames(played.num_frames());
                        callback(&nothing, played, info);
                    }),
                ))
            }
            _ => {
                let mut nothing = AudioBuffer::new(0, block);
                MockKind::Input(BlockCapturer::new(
                    format.input_channels,
                    block,
                    BlockClock::new(sample_rate, Arc::clone(&input_health)),
                    Box::new(move |captured: &AudioBuffer<f32>, info: &CallbackInfo| {
                        nothing.set_num_frames(captured.num_frames());
                        callback(captured, &mut nothing, info);
                    }),
                ))
            }
        };

        let token = state.next_token;
        state.next_token += 1;
        let side = |opened: &Option<(DeviceSummary, StreamSettings)>,
                    health: &Arc<StreamHealth>| {
            opened.as_ref().map(|(device, settings)| OpenedEndpoint {
                device: device.clone(),
                settings: *settings,
                health: Arc::clone(health),
            })
        };
        let started = Started {
            stream: Box::new(StreamGuard {
                state: Arc::downgrade(&self.state),
                token,
            }),
            format,
            input: side(&input, &input_health),
            output: side(&output, &output_health),
        };
        let keep = |opened: Option<(DeviceSummary, StreamSettings)>, health: Arc<StreamHealth>| {
            opened.map(|(device, settings)| (device.id, settings, health))
        };
        state.streams.push(MockStream {
            token,
            kind,
            errors: on_error,
            sample_rate,
            input: keep(input, input_health),
            output: keep(output, output_health),
            input_frames: 0,
            output_frames: 0,
        });
        Ok(started)
    }

    #[cfg(feature = "midi")]
    fn connect_midi_input(&self, id: &str, sink: MidiSink) -> Result<Box<dyn Send>, MidiError> {
        let mut state = self.state();
        if !state.midi_inputs.iter().any(|port| port.id == id) {
            return Err(MidiError::PortNotFound(id.to_owned()));
        }
        let token = state.next_token;
        state.next_token += 1;
        state.midi_connections.push(MockMidiConnection {
            token,
            port: id.to_owned(),
            sink,
            parser: StreamParser::new(0),
        });
        Ok(Box::new(MidiGuard {
            state: Arc::downgrade(&self.state),
            token,
        }))
    }

    #[cfg(feature = "midi")]
    fn connect_midi_output(&self, id: &str) -> Result<Box<dyn MidiSender>, MidiError> {
        let state = self.state();
        if !state.midi_outputs.iter().any(|port| port.id == id) {
            return Err(MidiError::PortNotFound(id.to_owned()));
        }
        Ok(Box::new(MockMidiOutput {
            state: Arc::downgrade(&self.state),
            port: id.to_owned(),
        }))
    }
}
