//! Runs a [`Plugin`] as a standalone application.
//!
//! The same plugin that is exported to a host can be played directly: its
//! audio goes to the sound card, its notes come from every connected MIDI
//! keyboard, and an effect takes its input from the default microphone or
//! line input. A plugin that sends notes ([`Plugin::NOTE_OUTPUT`]) can
//! send them to a MIDI output port, as MIDI 1.0 or MPE, with
//! [`Standalone::start_with_midi_output`].
//!
//! The plugin plays with a real transport: a [`TransportClock`] that
//! starts playing at [`StandaloneOptions::tempo_bpm`], and that the program
//! works through [`Standalone::transport`]: play, stop, seek, loop, and a
//! new tempo map with tempo changes, ramps and time signatures while it
//! plays.
//! The plugin sees it as a host's transport, and the map in
//! `ProcessContext::timeline`.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use manguebyte_core::AudioBuffer;
//! # use manguebyte_dsp::{ProcessContext, ProcessSpec};
//! # use manguebyte_params::param_set;
//! # use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
//! # struct NoParams {}
//! # param_set!(NoParams {});
//! # struct MySynth(Arc<NoParams>);
//! # impl Plugin for MySynth {
//! #     const INFO: PluginInfo = PluginInfo { id: "x", name: "x", vendor: "x", version: "1", description: "x", kind: PluginKind::Instrument };
//! #     const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
//! #     type Params = NoParams;
//! #     fn new() -> Self { Self(Arc::new(NoParams {})) }
//! #     fn params(&self) -> Arc<NoParams> { Arc::clone(&self.0) }
//! #     fn prepare(&mut self, _: &ProcessSpec) {}
//! #     fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
//! # }
//! use manguebyte_midi::MidiMessage;
//! use manguebyte_standalone::{Standalone, StandaloneOptions};
//!
//! let app = Standalone::<MySynth>::start(StandaloneOptions::default())?;
//! println!("playing on {} at {} Hz", app.device_name(), app.sample_rate());
//!
//! // Notes can also come from the program itself: a computer keyboard, an
//! // on-screen one, a sequencer.
//! app.send(MidiMessage::NoteOn { channel: 0, note: 60, velocity: 100 });
//! std::thread::sleep(std::time::Duration::from_secs(1));
//! # Ok::<(), manguebyte_standalone::StandaloneError>(())
//! ```

#![forbid(unsafe_code)]

mod engine;

use std::fmt;
use std::sync::{Arc, Mutex};

use manguebyte_core::spsc::{self, Producer};
use manguebyte_devices::midi::{
    MidiError, MidiInput, MidiOutput, MidiOutputThread, MidiProtocol, midi_input_ports,
    midi_output_queue,
};
use manguebyte_devices::{
    DeviceError, DuplexSettings, DuplexStream, InputDevice, OutputDevice, OutputStream,
    StreamSettings,
};
use manguebyte_dsp::timeline::{ClockControl, TempoMap, TimeSignature, TransportClock};
use manguebyte_midi::MidiMessage;
use manguebyte_midi::mpe::Zone;
use manguebyte_params::LoadReport;
use manguebyte_plugin::output::OutputProtocol;
use manguebyte_plugin::{Plugin, StateError, load_state, save_state};

/// How many packets can wait for the MIDI output thread: a dense part
/// with expressions sends a few thousand a second.
const MIDI_OUTPUT_QUEUE: usize = 8_192;

pub use engine::Engine;

/// How to run a plugin standalone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StandaloneOptions {
    /// Largest block the plugin will be asked to process, in frames.
    pub block_size: usize,
    /// What to ask the output device for. The default takes the device's
    /// own preferences.
    pub output: StreamSettings,
    /// Tempo the transport starts at, in beats per minute, in 4/4; 120 if
    /// it is not a positive, finite number. [`Standalone::transport`] can
    /// hand it a whole tempo map later.
    pub tempo_bpm: f64,
    /// Whether to listen to every MIDI input port that is connected.
    pub open_midi_inputs: bool,
    /// Where the plugin's auxiliary output buses go: for each bus in
    /// order, the device channel of its first channel, or `None` to leave
    /// it unheard, as [`Engine::with_aux_channels`] takes them. Empty, the
    /// default, sends each bus that is on by default to the channels after
    /// the main output's, one bus after another.
    pub aux_channels: &'static [Option<usize>],
}

impl Default for StandaloneOptions {
    fn default() -> Self {
        Self {
            block_size: 512,
            output: StreamSettings::default(),
            tempo_bpm: 120.0,
            open_midi_inputs: true,
            aux_channels: &[],
        }
    }
}

/// Why a plugin could not be started standalone.
#[derive(Debug)]
pub enum StandaloneError {
    /// The audio device could not be opened or started.
    Device(DeviceError),
    /// The plugin's parameters are not well formed.
    InvalidPlugin(String),
}

impl fmt::Display for StandaloneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device(error) => write!(f, "{error}"),
            Self::InvalidPlugin(reason) => write!(f, "the plugin is not well formed: {reason}"),
        }
    }
}

impl std::error::Error for StandaloneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Device(error) => Some(error),
            Self::InvalidPlugin(_) => None,
        }
    }
}

impl From<DeviceError> for StandaloneError {
    fn from(error: DeviceError) -> Self {
        Self::Device(error)
    }
}

/// Keeps whichever kind of stream is running alive. The streams are never
/// used again; holding them is what keeps the audio going.
#[allow(dead_code)]
enum Stream {
    Output(OutputStream),
    Duplex(DuplexStream),
}

/// A plugin running on the sound card. Audio stops when it is dropped.
pub struct Standalone<P: Plugin> {
    params: Arc<P::Params>,
    notes: Mutex<Producer<MidiMessage>>,
    device_name: String,
    input_name: Option<String>,
    sample_rate: f64,
    midi_inputs: Vec<MidiInput>,
    midi_problems: Vec<MidiError>,
    transport: ClockControl,
    _stream: Stream,
    /// The thread that sends the plugin's notes to the MIDI output port,
    /// and the port's name. After the stream, which feeds it, so that it
    /// is dropped after it, sending what the last blocks left.
    midi_output: Option<(MidiOutputThread, String)>,
}

/// Where a plugin that sends notes sends them when it runs standalone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiOutputOptions {
    /// The output port, as [`MidiPortInfo::id`] names it.
    ///
    /// [`MidiPortInfo::id`]: manguebyte_devices::midi::MidiPortInfo::id
    pub port_id: String,
    /// For a port that speaks MIDI 1.0, as every port does for now
    /// ([`MidiProtocol`]): `Some` writes the plugin's notes as MPE in this
    /// zone, so that each keeps its own tuning, pressure and brightness,
    /// and `None` as plain MIDI 1.0 by the default translation, which
    /// keeps polyphonic pressure alone of them.
    pub mpe: Option<Zone>,
}

impl MidiOutputOptions {
    /// Plain MIDI 1.0 to the port `port_id`.
    pub fn new(port_id: impl Into<String>) -> Self {
        Self {
            port_id: port_id.into(),
            mpe: None,
        }
    }

    /// MPE in a lower zone of all fifteen member channels instead.
    #[must_use]
    pub fn mpe(mut self) -> Self {
        self.mpe = Zone::lower(15);
        self
    }
}

impl<P: Plugin> Standalone<P> {
    /// Creates the plugin and starts playing it on the default output
    /// device.
    ///
    /// An effect also needs an input. If there is no input device, or it
    /// cannot run at the output's sample rate, the effect is started anyway
    /// and hears silence; [`input_name`](Self::input_name) says which
    /// happened. MIDI ports that cannot be opened are skipped and listed in
    /// [`midi_problems`](Self::midi_problems), because a missing keyboard
    /// should not stop the sound.
    pub fn start(options: StandaloneOptions) -> Result<Self, StandaloneError> {
        Self::launch(options, None)
    }

    /// [`start`](Self::start), and sends what the plugin sends out, from
    /// [`Plugin::process_with_output`], to a MIDI output port, each packet
    /// on time (see [`MidiTransmitter`](manguebyte_devices::midi::MidiTransmitter)).
    ///
    /// For a plugin that does not set [`Plugin::NOTE_OUTPUT`] the port is
    /// not opened. A port that cannot be opened is listed in
    /// [`midi_problems`](Self::midi_problems), and the plugin plays on.
    pub fn start_with_midi_output(
        options: StandaloneOptions,
        midi_output: MidiOutputOptions,
    ) -> Result<Self, StandaloneError> {
        Self::launch(options, Some(midi_output))
    }

    fn launch(
        options: StandaloneOptions,
        midi_output: Option<MidiOutputOptions>,
    ) -> Result<Self, StandaloneError> {
        let output = OutputDevice::open_default()?.with_settings(options.output)?;
        let sample_rate = output.sample_rate();

        let mut midi_inputs = Vec::new();
        let mut midi_problems = Vec::new();
        let mut receivers = Vec::new();
        if options.open_midi_inputs {
            match midi_input_ports() {
                Ok(ports) => {
                    for port in ports {
                        match MidiInput::open(&port.id) {
                            Ok((input, receiver)) => {
                                midi_inputs.push(input);
                                receivers.push(receiver);
                            }
                            Err(error) => midi_problems.push(error),
                        }
                    }
                }
                Err(error) => midi_problems.push(error),
            }
        }

        let (notes, note_receiver) = spsc::channel(1_024);
        let engine = Engine::<P>::new(
            sample_rate,
            options.block_size,
            options.tempo_bpm,
            receivers,
            note_receiver,
        )
        .map_err(StandaloneError::InvalidPlugin)?;
        let map = TempoMap::new(options.tempo_bpm, TimeSignature::FOUR_FOUR).unwrap_or_default();
        let (mut clock, transport) = TransportClock::with_control(map, sample_rate);
        clock.play();
        let mut engine = engine
            .with_transport_clock(clock)
            .with_aux_channels(options.aux_channels);
        let params = engine.params();

        let mut sender = None;
        if let Some(choice) = midi_output.filter(|_| P::NOTE_OUTPUT) {
            match MidiOutput::open(&choice.port_id) {
                Ok(port) => {
                    let protocol = match (port.protocol(), choice.mpe) {
                        (MidiProtocol::Midi1, Some(zone)) => OutputProtocol::Mpe(zone),
                        (MidiProtocol::Midi1, None) => OutputProtocol::Midi1,
                        // A port that speaks MIDI 2.0, when there are any.
                        _ => OutputProtocol::Midi2,
                    };
                    let (transmitter, queue) = midi_output_queue(MIDI_OUTPUT_QUEUE);
                    engine = engine.with_midi_output(transmitter, protocol);
                    let name = port.name().to_owned();
                    sender = Some((port.spawn(queue), name));
                }
                Err(error) => midi_problems.push(error),
            }
        }

        // An effect is given an input if one can run alongside the output.
        let input = (P::LAYOUT.inputs > 0)
            .then(|| {
                InputDevice::open_default()
                    .and_then(|device| {
                        device.with_settings(
                            StreamSettings::default().with_sample_rate(sample_rate as u32),
                        )
                    })
                    .ok()
            })
            .flatten();

        let (stream, input_name) = match input {
            Some(input) => {
                let name = input.name().to_owned();
                let stream = DuplexStream::start_with(
                    &input,
                    &output,
                    DuplexSettings::new(options.block_size),
                    move |captured, buffer| engine.render(Some(captured), buffer),
                    |error| eprintln!("manguebyte: audio stream error: {error}"),
                )?;
                (Stream::Duplex(stream), Some(name))
            }
            None => {
                let stream = output.start(options.block_size, move |buffer| {
                    engine.render(None, buffer);
                })?;
                (Stream::Output(stream), None)
            }
        };

        Ok(Self {
            params,
            notes: Mutex::new(notes),
            device_name: output.name().to_owned(),
            input_name,
            sample_rate,
            midi_inputs,
            midi_problems,
            transport,
            midi_output: sender,
            _stream: stream,
        })
    }

    /// The plugin's parameters. Writing to them changes the sound at once.
    pub fn params(&self) -> &P::Params {
        &self.params
    }

    /// Plays a MIDI message as if it had come from a keyboard, at the start
    /// of the next block. Returns `false` if too many messages are already
    /// waiting.
    pub fn send(&self, message: MidiMessage) -> bool {
        let mut notes = self
            .notes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        notes.push(message).is_ok()
    }

    /// The transport: play, stop, seek, loop, a new tempo map, and where
    /// it is. Commands take effect at the start of the next block.
    pub fn transport(&self) -> &ClockControl {
        &self.transport
    }

    /// Name of the output device.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Name of the input device, if the plugin is an effect and one was
    /// opened.
    pub fn input_name(&self) -> Option<&str> {
        self.input_name.as_deref()
    }

    /// Sample rate the plugin is running at, in hertz.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Names of the MIDI input ports being listened to.
    pub fn midi_input_names(&self) -> Vec<&str> {
        self.midi_inputs.iter().map(MidiInput::name).collect()
    }

    /// MIDI ports that could not be opened.
    pub fn midi_problems(&self) -> &[MidiError] {
        &self.midi_problems
    }

    /// Name of the MIDI output port the plugin's notes are sent to, if one
    /// is open.
    pub fn midi_output_name(&self) -> Option<&str> {
        self.midi_output.as_ref().map(|(_, name)| name.as_str())
    }

    /// How many messages the MIDI output port refused, if one is open.
    pub fn midi_output_errors(&self) -> Option<u64> {
        self.midi_output.as_ref().map(|(thread, _)| thread.errors())
    }

    /// Captures the plugin's state as text.
    pub fn save_state(&self) -> String {
        save_state::<P>(&self.params)
    }

    /// Restores state captured by [`save_state`](Self::save_state).
    pub fn load_state(&self, text: &str) -> Result<LoadReport, StateError> {
        load_state::<P>(&self.params, text)
    }
}
