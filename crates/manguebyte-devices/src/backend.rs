//! What the device manager and the watcher need from the system, so that a
//! headless stand-in can take its place in tests.

use std::sync::{Arc, Mutex};

use manguebyte_core::AudioBuffer;

use crate::device::Direction;
use crate::hotplug::{DeviceSnapshot, DeviceSummary};
#[cfg(feature = "midi")]
use crate::midi::{MidiError, MidiOutput, MidiProtocol, MidiSink};
use crate::timing::StreamHealth;
use crate::{
    AudioHost, CallbackInfo, DeviceError, DeviceInfo, DuplexSettings, DuplexStream, HostInfo,
    InputDevice, OutputDevice, StreamSettings, audio_hosts,
};

/// The callback a backend runs: captured audio in, audio to play out.
/// Either buffer may have no channels.
pub(crate) type EngineCallback =
    Box<dyn FnMut(&AudioBuffer<f32>, &mut AudioBuffer<f32>, &CallbackInfo) + Send>;

/// Where a backend reports stream errors. Called off the path that moves
/// audio.
pub(crate) type ErrorSink = Arc<dyn Fn(DeviceError) + Send + Sync>;

/// How a running stream's devices are configured, known once they are
/// open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct OpenedFormat {
    pub(crate) sample_rate: u32,
    pub(crate) input_channels: usize,
    pub(crate) output_channels: usize,
}

/// One side of a stream to start.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Endpoint {
    pub(crate) id: String,
    pub(crate) settings: StreamSettings,
}

/// A stream to start: an input, an output, or both.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StreamRequest {
    pub(crate) input: Option<Endpoint>,
    pub(crate) output: Option<Endpoint>,
    pub(crate) duplex: DuplexSettings,
}

/// One side of a running stream.
pub(crate) struct OpenedEndpoint {
    pub(crate) device: DeviceSummary,
    pub(crate) settings: StreamSettings,
    pub(crate) health: Arc<StreamHealth>,
}

/// A running stream. Dropping `stream` stops it and drops the callback.
pub(crate) struct Started {
    pub(crate) stream: Box<dyn Send>,
    pub(crate) format: OpenedFormat,
    pub(crate) input: Option<OpenedEndpoint>,
    pub(crate) output: Option<OpenedEndpoint>,
}

/// Sends MIDI to one output port.
#[cfg(feature = "midi")]
pub(crate) trait MidiSender: Send {
    /// Sends one complete MIDI 1.0 message.
    fn send(&mut self, bytes: &[u8]) -> Result<(), MidiError>;

    /// The protocol the port speaks. A port that speaks MIDI 2.0 is sent
    /// packets through [`send_ump`](Self::send_ump) and
    /// [`flush`](Self::flush) instead of [`send`](Self::send).
    fn protocol(&self) -> MidiProtocol {
        MidiProtocol::Midi1
    }

    /// Takes one whole Universal MIDI Packet, unchanged, for a port that
    /// speaks MIDI 2.0. It may wait for [`flush`](Self::flush).
    fn send_ump(&mut self, words: &[u32]) -> Result<(), MidiError> {
        let _ = words;
        Err(MidiError::Send(String::from("the port speaks MIDI 1.0")))
    }

    /// Hands the system what [`send_ump`](Self::send_ump) took.
    fn flush(&mut self) -> Result<(), MidiError> {
        Ok(())
    }
}

#[cfg(feature = "midi")]
impl MidiSender for MidiOutput {
    fn send(&mut self, bytes: &[u8]) -> Result<(), MidiError> {
        self.send_raw(bytes)
    }

    fn protocol(&self) -> MidiProtocol {
        MidiOutput::protocol(self)
    }

    fn send_ump(&mut self, words: &[u32]) -> Result<(), MidiError> {
        self.send_ump_words(words)
    }

    fn flush(&mut self) -> Result<(), MidiError> {
        self.flush_ump()
    }
}

/// The system, as the device manager sees it.
pub(crate) trait Backend: Send + Sync {
    /// The name of the audio host in use.
    fn host_name(&self) -> String;

    /// The hosts that could be used.
    fn hosts(&self) -> Vec<HostInfo>;

    /// Switches to the host with this name, or to the default one.
    fn select_host(&self, name: Option<&str>) -> Result<(), DeviceError>;

    /// What is connected now. Cheap: called on every poll.
    fn snapshot(&self) -> Result<DeviceSnapshot, DeviceError>;

    /// Everything about one device, with its configurations.
    fn device_info(&self, direction: Direction, id: &str) -> Result<DeviceInfo, DeviceError>;

    /// Whether a second stream can be started while one runs, on another
    /// device, so that the manager can crossfade from one to the other.
    fn concurrent_streams(&self) -> bool;

    /// Opens the devices of `request`, hands their format to `make`, which
    /// returns the callback, and starts the stream. If anything fails after
    /// `make` was called, the callback it returned is dropped.
    fn start(
        &self,
        request: &StreamRequest,
        make: &mut dyn FnMut(OpenedFormat) -> EngineCallback,
        on_error: ErrorSink,
    ) -> Result<Started, DeviceError>;

    /// Connects a MIDI input port to `sink`. Receiving stops when the
    /// returned connection is dropped.
    #[cfg(feature = "midi")]
    fn connect_midi_input(&self, id: &str, sink: MidiSink) -> Result<Box<dyn Send>, MidiError>;

    /// Opens a MIDI output port.
    #[cfg(feature = "midi")]
    fn connect_midi_output(&self, id: &str) -> Result<Box<dyn MidiSender>, MidiError>;
}

/// The real system, through `cpal` and `midir`.
pub(crate) struct SystemBackend {
    host: Mutex<AudioHost>,
}

impl SystemBackend {
    pub(crate) fn new(host: AudioHost) -> Self {
        Self {
            host: Mutex::new(host),
        }
    }

    fn host(&self) -> AudioHost {
        match self.host.lock() {
            Ok(host) => *host,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

fn opened(
    id: &str,
    name: &str,
    settings: StreamSettings,
    health: &Arc<StreamHealth>,
) -> OpenedEndpoint {
    OpenedEndpoint {
        device: DeviceSummary {
            id: id.to_owned(),
            name: name.to_owned(),
        },
        settings,
        health: Arc::clone(health),
    }
}

impl Backend for SystemBackend {
    fn host_name(&self) -> String {
        self.host().name().to_owned()
    }

    fn hosts(&self) -> Vec<HostInfo> {
        audio_hosts()
    }

    fn select_host(&self, name: Option<&str>) -> Result<(), DeviceError> {
        let host = match name {
            Some(name) => AudioHost::by_name(name)?,
            None => AudioHost::default_host(),
        };
        if let Ok(mut current) = self.host.lock() {
            *current = host;
        }
        Ok(())
    }

    fn snapshot(&self) -> Result<DeviceSnapshot, DeviceError> {
        let host = self.host();
        let (outputs, default_output) = host.summaries(Direction::Output)?;
        let (inputs, default_input) = host.summaries(Direction::Input)?;
        let mut snapshot = DeviceSnapshot {
            outputs,
            inputs,
            default_output,
            default_input,
            midi_inputs: Vec::new(),
            midi_outputs: Vec::new(),
        };
        list_system_midi(&mut snapshot);
        Ok(snapshot)
    }

    fn device_info(&self, direction: Direction, id: &str) -> Result<DeviceInfo, DeviceError> {
        let devices = match direction {
            Direction::Output => self.host().output_devices()?,
            Direction::Input => self.host().input_devices()?,
        };
        devices
            .into_iter()
            .find(|device| device.id == id)
            .ok_or_else(|| DeviceError::DeviceNotFound(id.to_owned()))
    }

    fn concurrent_streams(&self) -> bool {
        // Every host runs several streams, except ASIO, whose drivers are
        // loaded one per process.
        self.host().name() != "ASIO"
    }

    fn start(
        &self,
        request: &StreamRequest,
        make: &mut dyn FnMut(OpenedFormat) -> EngineCallback,
        on_error: ErrorSink,
    ) -> Result<Started, DeviceError> {
        let report = move |error| on_error(error);
        let block = request.duplex.max_block_size.max(1);
        let input = request
            .input
            .as_ref()
            .map(|endpoint| InputDevice::open_by_id(&endpoint.id)?.with_settings(endpoint.settings))
            .transpose()?;
        let output = request
            .output
            .as_ref()
            .map(|endpoint| {
                OutputDevice::open_by_id(&endpoint.id)?.with_settings(endpoint.settings)
            })
            .transpose()?;
        let format = OpenedFormat {
            sample_rate: output
                .as_ref()
                .map(|device| device.opened.sample_rate())
                .or(input.as_ref().map(|device| device.opened.sample_rate()))
                .unwrap_or(0),
            input_channels: input.as_ref().map_or(0, InputDevice::num_channels),
            output_channels: output.as_ref().map_or(0, OutputDevice::num_channels),
        };

        match (input, output) {
            (Some(input), Some(output)) => {
                let mut callback = make(format);
                let stream = DuplexStream::start_timed(
                    &input,
                    &output,
                    request.duplex,
                    move |captured, played, info| callback(captured, played, info),
                    report,
                )?;
                Ok(Started {
                    input: Some(opened(
                        input.id(),
                        input.name(),
                        input.opened.settings(),
                        &stream.input_health,
                    )),
                    output: Some(opened(
                        output.id(),
                        output.name(),
                        output.opened.settings(),
                        &stream.output_health,
                    )),
                    stream: Box::new(stream),
                    format,
                })
            }
            (None, Some(output)) => {
                let mut callback = make(format);
                let mut nothing = AudioBuffer::new(0, block);
                let stream = output.start_timed(
                    block,
                    move |played, info| {
                        nothing.set_num_frames(played.num_frames());
                        callback(&nothing, played, info);
                    },
                    report,
                )?;
                Ok(Started {
                    input: None,
                    output: Some(opened(
                        output.id(),
                        output.name(),
                        output.opened.settings(),
                        &stream.health,
                    )),
                    stream: Box::new(stream),
                    format,
                })
            }
            (Some(input), None) => {
                let mut callback = make(format);
                let mut nothing = AudioBuffer::new(0, block);
                let stream = input.start_timed(
                    block,
                    move |captured, info| {
                        nothing.set_num_frames(captured.num_frames());
                        callback(captured, &mut nothing, info);
                    },
                    report,
                )?;
                Ok(Started {
                    input: Some(opened(
                        input.id(),
                        input.name(),
                        input.opened.settings(),
                        &stream.health,
                    )),
                    output: None,
                    stream: Box::new(stream),
                    format,
                })
            }
            (None, None) => Err(DeviceError::UnsupportedSettings(String::from(
                "neither an input nor an output was asked for",
            ))),
        }
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

/// Fills in the system's MIDI ports, which every backend but the mock
/// shares.
pub(crate) fn list_system_midi(snapshot: &mut DeviceSnapshot) {
    #[cfg(feature = "midi")]
    {
        // No MIDI service is no MIDI ports, not a failure to list audio.
        let summary = |port: crate::midi::MidiPortInfo| DeviceSummary {
            id: port.id,
            name: port.name,
        };
        snapshot.midi_inputs = crate::midi::midi_input_ports()
            .unwrap_or_default()
            .into_iter()
            .map(summary)
            .collect();
        snapshot.midi_outputs = crate::midi::midi_output_ports()
            .unwrap_or_default()
            .into_iter()
            .map(summary)
            .collect();
    }
    #[cfg(not(feature = "midi"))]
    let _ = snapshot;
}

#[cfg(feature = "midi")]
pub(crate) fn connect_system_midi_input(
    id: &str,
    sink: MidiSink,
) -> Result<Box<dyn Send>, MidiError> {
    let (connection, _) = crate::midi::connect_input(id, sink)?;
    Ok(Box::new(connection))
}

#[cfg(feature = "midi")]
pub(crate) fn connect_system_midi_output(id: &str) -> Result<Box<dyn MidiSender>, MidiError> {
    Ok(Box::new(MidiOutput::open(id)?))
}
