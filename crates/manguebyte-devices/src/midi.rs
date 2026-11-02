//! MIDI input and output ports.
//!
//! ```no_run
//! use manguebyte_devices::midi::{MidiInput, MidiInputMessage, midi_input_ports};
//!
//! let ports = midi_input_ports()?;
//! let Some(port) = ports.first() else {
//!     println!("no MIDI inputs");
//!     return Ok(());
//! };
//! let (_connection, mut receiver) = MidiInput::open(&port.id)?;
//!
//! // In the audio callback, once per block:
//! let (frames, sample_rate) = (512, 48_000.0);
//! receiver.read(frames, sample_rate, |offset, message| {
//!     if let MidiInputMessage::Channel(message) = message {
//!         println!("frame {offset}: {message:?}");
//!     }
//! });
//! # Ok::<(), manguebyte_devices::midi::MidiError>(())
//! ```
//!
//! # Timing
//!
//! MIDI arrives on a system thread, at any moment. The audio callback runs
//! in bursts, one block at a time. Handing a note to the audio thread "as
//! soon as possible" would snap every note to the start of a block, which at
//! a buffer of 1024 frames is more than 20 ms of jitter: enough to make a
//! good player sound sloppy.
//!
//! Instead each message is stamped with the time it arrived, and
//! [`MidiReceiver::read`] places it inside the block at the offset that
//! matches how long after the previous block it came. Every note is late by
//! one block, and by the same amount, so the player's timing is kept.
//!
//! # Output
//!
//! [`MidiOutput`] sends to a port. From the audio thread, packets go
//! through a [`midi_output_queue`]: its [`MidiTransmitter`] stamps each
//! with the moment its frame is heard, one block late as input is, and
//! [`MidiOutput::spawn`] starts the thread that sends them then. The audio
//! thread's side is the same whatever the port speaks. Under a
//! [`DeviceManager`](crate::DeviceManager),
//! [`DeviceManager::midi_output_transmitter`](crate::DeviceManager::midi_output_transmitter)
//! makes the queue to the manager's own output port instead, which can
//! then change, or come and go, while the audio thread sends, without
//! leaving a note hanging on the port it leaves.
//!
//! # MIDI 1.0 and MIDI 2.0 ports
//!
//! A port speaks MIDI 1.0 unless the application asks for MIDI 2.0 with
//! [`MidiOutput::open_with_protocol`] and both the system and the port
//! speak it ([`MidiProtocol`]):
//!
//! | System | MIDI 2.0 port | Every other port |
//! |---|---|---|
//! | macOS 11, iOS 14 and newer | CoreMIDI's `MIDISendEventList`, an event list in `kMIDIProtocol_2_0`, to a destination whose `kMIDIPropertyProtocolID` is MIDI 2.0 | CoreMIDI's MIDI 1.0 calls |
//! | Linux 6.5 with alsa-lib 1.2.10 and newer | the ALSA sequencer's UMP events, from a MIDI 2.0 client to a port of a MIDI 2.0 client | the sequencer's MIDI 1.0 events |
//! | Windows | none: Windows MIDI Services' API is not reachable ([`ump_output_backend`]) | WinMM, which the MIDI service, where installed, translates for MIDI 2.0 devices |
//! | Android, older systems | none | the system's MIDI 1.0 API |
//!
//! To a MIDI 2.0 port every packet goes unchanged, per-note controllers,
//! attributes and 16-bit velocities included. To a MIDI 1.0 port packets
//! go out by the specification's default translation ([`PacketWriter`]);
//! for what shapes single notes there, write MPE first, as
//! `manguebyte_plugin::output::OutputTranslator` does.
//! [`MidiOutput::fallback`] says why a port speaks MIDI 1.0
//! ([`ProtocolFallback`]), and [`midi_output_ports_with_protocols`] says it
//! for every port before one is opened. What is missing on a system is
//! found when the program runs, not when it is built, so a program built
//! here speaks MIDI 1.0 on an older system and says why.
//!
//! The foreign calls are made by the workspace crate `manguebyte-ump-io`,
//! since this crate forbids `unsafe` code.
//!
//! ## Timing per system
//!
//! The sending thread wakes when a packet is due, or at the latest every
//! millisecond, and hands the system what is due then; packets due
//! together go in one call (one CoreMIDI event list). How late a packet
//! leaves is how late the thread wakes:
//!
//! - **macOS and iOS:** sleeps end within about a millisecond. Event lists
//!   carry the time stamp zero, "now"; CoreMIDI's scheduling ahead on its
//!   host clock is not used.
//! - **Linux:** sleeps end within about a millisecond. Events are written
//!   direct, not through a sequencer queue with time stamps.
//! - **Windows:** MIDI 1.0 only, and a sleep is rounded to the system
//!   timer, as much as 15.6 ms unless something has raised its resolution.
//!
//! # Virtual ports
//!
//! [`MidiInput::create_virtual`] and [`MidiOutput::create_virtual`] make a
//! port that other programs see and connect to, as if this program were a
//! MIDI device. macOS, iOS, Linux and the BSDs allow it. Windows has no
//! such ports in its MIDI API (a loopback driver such as loopMIDI has to
//! be installed), and Android lets only a Java `MidiDeviceService` offer
//! one; there both functions return [`MidiError::VirtualPortsUnsupported`].

use std::fmt;
use std::sync::{Arc, Mutex};

mod output;
mod shared;
mod ump;

pub use output::{
    MidiOutputQueue, MidiOutputThread, MidiProtocol, MidiTransmitter, PacketWriter,
    SYSEX_OUT_CAPACITY, midi_output_queue,
};
pub(crate) use shared::{SharedOutput, SharedOutputThread};
use std::time::{Duration, Instant};
pub use ump::{MidiOutputPortInfo, ProtocolFallback, ump_output_backend};

use manguebyte_core::spsc::{self, Consumer, Producer};
use manguebyte_dsp::event::EventBuffer;
use manguebyte_dsp::note_input::NoteInput;
use manguebyte_midi::ump::{Message as UmpMessage, Packet, sysex7_packets};
use manguebyte_midi::{MidiMessage, StreamEvent, StreamParser, SystemMessage};
use ump::UmpPort;

/// How many messages can wait for the audio thread. A dense performance
/// with controllers produces a few hundred a second.
const QUEUE_CAPACITY: usize = 4_096;

const CLIENT_NAME: &str = "Manguebyte";

/// What went wrong with a MIDI port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiError {
    /// The system's MIDI service could not be reached.
    Unavailable,
    /// No port with this identifier is connected.
    PortNotFound(String),
    /// The port exists but could not be opened.
    Connect(String),
    /// A message could not be sent.
    Send(String),
    /// This system cannot create virtual MIDI ports. See the
    /// [module documentation](self#virtual-ports).
    VirtualPortsUnsupported,
}

impl fmt::Display for MidiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("the system's MIDI service is not available"),
            Self::PortNotFound(id) => write!(f, "no MIDI port with id \"{id}\""),
            Self::Connect(reason) => write!(f, "could not open the MIDI port: {reason}"),
            Self::Send(reason) => write!(f, "could not send the MIDI message: {reason}"),
            Self::VirtualPortsUnsupported => {
                f.write_str("this system cannot create virtual MIDI ports")
            }
        }
    }
}

impl std::error::Error for MidiError {}

/// A MIDI port that can be opened.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MidiPortInfo {
    /// Stable identifier, for reopening the same port in a later run.
    pub id: String,
    /// Human-readable name.
    pub name: String,
}

/// The identifier this crate gives a port.
///
/// The system's own identifier is used when there is one. Some ports have
/// none: on Windows the built-in software synthesiser reports an empty
/// string. Those are identified by their name instead, so that every port
/// can be stored and reopened.
fn port_id(system_id: &str, name: &str) -> String {
    if system_id.is_empty() {
        format!("name:{name}")
    } else {
        system_id.to_owned()
    }
}

fn port_name(name: Result<String, midir::PortInfoError>) -> String {
    name.unwrap_or_else(|_| String::from("unknown port"))
}

/// Lists the MIDI ports that can be received from.
pub fn midi_input_ports() -> Result<Vec<MidiPortInfo>, MidiError> {
    let input = midir::MidiInput::new(CLIENT_NAME).map_err(|_| MidiError::Unavailable)?;
    Ok(input
        .ports()
        .iter()
        .map(|port| {
            let name = port_name(input.port_name(port));
            MidiPortInfo {
                id: port_id(&port.id(), &name),
                name,
            }
        })
        .collect())
}

/// Lists the MIDI ports that can be sent to.
pub fn midi_output_ports() -> Result<Vec<MidiPortInfo>, MidiError> {
    let output = midir::MidiOutput::new(CLIENT_NAME).map_err(|_| MidiError::Unavailable)?;
    Ok(output
        .ports()
        .iter()
        .map(|port| {
            let name = port_name(output.port_name(port));
            MidiPortInfo {
                id: port_id(&port.id(), &name),
                name,
            }
        })
        .collect())
}

/// A message received from a port.
///
/// System exclusive messages are not delivered here: they have no fixed
/// size, and carrying them to the audio thread would mean freeing memory
/// there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiInputMessage {
    /// A note, controller, pitch bend or other channel message.
    Channel(MidiMessage),
    /// Clock, transport and the other system messages.
    System(SystemMessage),
}

/// A message and the moment it arrived.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timed {
    at: Instant,
    message: MidiInputMessage,
}

/// Where a port's messages go: a queue of its own, or one that several
/// ports share. The shared queue's lock is taken by the threads the system
/// delivers MIDI on, never by the audio thread, which only reads.
pub(crate) enum MidiSink {
    Own(Producer<Timed>),
    Shared(Arc<Mutex<Producer<Timed>>>),
}

impl MidiSink {
    /// Parses `bytes`, which arrived just now, and queues every message.
    pub(crate) fn deliver(&mut self, bytes: &[u8], parser: &mut StreamParser) {
        self.deliver_with_sysex(bytes, parser, |_| {});
    }

    /// [`deliver`](Self::deliver), handing each whole system exclusive
    /// message to `on_sysex` too, without its `0xF0` and `0xF7`.
    pub(crate) fn deliver_with_sysex(
        &mut self,
        bytes: &[u8],
        parser: &mut StreamParser,
        mut on_sysex: impl FnMut(&[u8]),
    ) {
        let at = Instant::now();
        let mut push = |message| {
            let timed = Timed { at, message };
            // A full queue means the audio thread has stopped reading.
            // Dropping is all that can be done.
            match self {
                Self::Own(producer) => {
                    let _ = producer.push(timed);
                }
                Self::Shared(shared) => {
                    if let Ok(mut producer) = shared.lock() {
                        let _ = producer.push(timed);
                    }
                }
            }
        };
        parser.feed_slice(bytes, |event| match event {
            StreamEvent::Channel(message) => push(MidiInputMessage::Channel(message)),
            StreamEvent::System(message) => push(MidiInputMessage::System(message)),
            StreamEvent::SysEx(payload) => on_sysex(payload),
        });
    }
}

/// The largest system exclusive message [`MidiInput::open_with_sysex`]
/// delivers, in bytes: room for a MIDI-CI property exchange chunk of the
/// largest size a device may announce in practice.
pub const SYSEX_CAPACITY: usize = 65_536;

/// One receiver for any number of ports: the sink to share between the
/// ports, and the audio thread's end.
pub(crate) fn merged_receiver() -> (Arc<Mutex<Producer<Timed>>>, MidiReceiver) {
    let (sender, receiver) = receiver_pair(QUEUE_CAPACITY);
    (Arc::new(Mutex::new(sender)), receiver)
}

/// The system's input port with this identifier, and its name.
fn find_input(
    input: &midir::MidiInput,
    id: &str,
) -> Result<(midir::MidiInputPort, String), MidiError> {
    input
        .ports()
        .into_iter()
        .map(|port| {
            let name = port_name(input.port_name(&port));
            (port, name)
        })
        .find(|(port, name)| port_id(&port.id(), name) == id)
        .ok_or_else(|| MidiError::PortNotFound(id.to_owned()))
}

fn new_input() -> Result<midir::MidiInput, MidiError> {
    let mut input = midir::MidiInput::new(CLIENT_NAME).map_err(|_| MidiError::Unavailable)?;
    input.ignore(midir::Ignore::SysexAndActiveSense);
    Ok(input)
}

/// A connection to an input port. Receiving stops when it is dropped.
pub(crate) type InputConnection = midir::MidiInputConnection<StreamParser>;

/// Connects the input port with this identifier to `sink`, and returns the
/// connection and the port's name.
pub(crate) fn connect_input(
    id: &str,
    mut sink: MidiSink,
) -> Result<(InputConnection, String), MidiError> {
    let input = new_input()?;
    let (port, name) = find_input(&input, id)?;
    let connection = input
        .connect(
            &port,
            "manguebyte-in",
            move |_timestamp, bytes, parser: &mut StreamParser| sink.deliver(bytes, parser),
            StreamParser::new(0),
        )
        .map_err(|error| MidiError::Connect(error.to_string()))?;
    Ok((connection, name))
}

/// An open MIDI input port. Receiving stops when it is dropped.
pub struct MidiInput {
    name: String,
    _connection: midir::MidiInputConnection<StreamParser>,
}

impl MidiInput {
    /// Opens the input port with this identifier, as found in
    /// [`MidiPortInfo::id`].
    ///
    /// Returns the connection, which must be kept alive, and the receiver,
    /// which goes to the audio thread.
    pub fn open(id: &str) -> Result<(Self, MidiReceiver), MidiError> {
        let (sender, receiver) = receiver_pair(QUEUE_CAPACITY);
        let (connection, name) = connect_input(id, MidiSink::Own(sender))?;
        Ok((
            Self {
                name,
                _connection: connection,
            },
            receiver,
        ))
    }

    /// Opens the input port with this identifier, as [`open`](Self::open)
    /// does, and also hands every system exclusive message that arrives on
    /// it to `on_sysex`, without its `0xF0` and `0xF7`, such as the MIDI-CI
    /// messages a [`manguebyte_midi::ci::agent::Agent`] reads.
    ///
    /// `on_sysex` runs on the thread the system delivers MIDI on, never on
    /// the audio thread, and may take its time there; a message longer
    /// than [`SYSEX_CAPACITY`] is dropped. Send replies with
    /// [`MidiOutput::send_raw`].
    pub fn open_with_sysex(
        id: &str,
        mut on_sysex: impl FnMut(&[u8]) + Send + 'static,
    ) -> Result<(Self, MidiReceiver), MidiError> {
        let (sender, receiver) = receiver_pair(QUEUE_CAPACITY);
        let mut sink = MidiSink::Own(sender);
        let mut input = midir::MidiInput::new(CLIENT_NAME).map_err(|_| MidiError::Unavailable)?;
        input.ignore(midir::Ignore::ActiveSense);
        let (port, name) = find_input(&input, id)?;
        let connection = input
            .connect(
                &port,
                "manguebyte-in",
                move |_timestamp, bytes, parser: &mut StreamParser| {
                    sink.deliver_with_sysex(bytes, parser, &mut on_sysex);
                },
                StreamParser::new(SYSEX_CAPACITY),
            )
            .map_err(|error| MidiError::Connect(error.to_string()))?;
        Ok((
            Self {
                name,
                _connection: connection,
            },
            receiver,
        ))
    }

    /// Creates a virtual input port called `name`, which other programs
    /// see as a MIDI output they can send to.
    ///
    /// Fails with [`MidiError::VirtualPortsUnsupported`] on Windows and
    /// Android; see the [module documentation](self#virtual-ports).
    pub fn create_virtual(name: &str) -> Result<(Self, MidiReceiver), MidiError> {
        #[cfg(all(unix, not(target_os = "android")))]
        {
            use midir::os::unix::VirtualInput;

            let input = new_input()?;
            let (sender, receiver) = receiver_pair(QUEUE_CAPACITY);
            let mut sink = MidiSink::Own(sender);
            let connection = input
                .create_virtual(
                    name,
                    move |_timestamp, bytes, parser: &mut StreamParser| sink.deliver(bytes, parser),
                    StreamParser::new(0),
                )
                .map_err(|error| MidiError::Connect(error.to_string()))?;
            Ok((
                Self {
                    name: name.to_owned(),
                    _connection: connection,
                },
                receiver,
            ))
        }
        #[cfg(not(all(unix, not(target_os = "android"))))]
        {
            let _ = name;
            Err(MidiError::VirtualPortsUnsupported)
        }
    }

    /// Human-readable name of the port.
    pub fn name(&self) -> &str {
        &self.name
    }
}

fn receiver_pair(capacity: usize) -> (Producer<Timed>, MidiReceiver) {
    let (sender, queue) = spsc::channel(capacity);
    (
        sender,
        MidiReceiver {
            queue,
            previous: None,
        },
    )
}

/// The audio thread's end of a MIDI input.
///
/// See the [module documentation](self) for how it keeps a player's timing.
pub struct MidiReceiver {
    queue: Consumer<Timed>,
    /// When the previous block was read.
    previous: Option<Instant>,
}

impl MidiReceiver {
    /// Hands over every message that has arrived, each with its frame
    /// inside the coming block of `frames` frames. Call it once per block.
    ///
    /// Real-time safe: it reads a wait-free queue and the clock.
    pub fn read(
        &mut self,
        frames: usize,
        sample_rate: f64,
        handle: impl FnMut(u32, MidiInputMessage),
    ) {
        self.read_at(Instant::now(), frames, sample_rate, handle);
    }

    /// Hands over every channel message that has arrived as the
    /// framework's events, read through `input`, each on its frame inside
    /// the coming block. Call it once per block, instead of
    /// [`read`](Self::read).
    ///
    /// `input` turns MPE and plain MIDI into notes, per-note expressions
    /// and the channel-wide messages that remain, as every plugin wrapper
    /// does (see [`NoteInput`]). Keep one `NoteInput` per stream of
    /// messages: it remembers the zones and the notes that are held. What
    /// the buffer has no room for is dropped. System messages are not
    /// events and are dropped too; use [`read`](Self::read) for them.
    ///
    /// Real-time safe.
    pub fn read_events(
        &mut self,
        frames: usize,
        sample_rate: f64,
        input: &mut NoteInput,
        events: &mut EventBuffer,
    ) {
        self.read(frames, sample_rate, |offset, message| {
            if let MidiInputMessage::Channel(message) = message {
                input.midi1(offset, message, |event| {
                    let _ = events.push(event);
                });
            }
        });
    }

    /// Forgets when the previous block was read, for a stream that
    /// starts again after a pause.
    pub(crate) fn restart(&mut self) {
        self.previous = None;
    }

    /// [`read`](Self::read), with the current time passed in.
    fn read_at(
        &mut self,
        now: Instant,
        frames: usize,
        sample_rate: f64,
        mut handle: impl FnMut(u32, MidiInputMessage),
    ) {
        let block = Duration::from_secs_f64(frames as f64 / sample_rate.max(1.0));
        // On the first block, pretend the previous one was exactly one
        // block ago.
        let previous = self
            .previous
            .unwrap_or_else(|| now.checked_sub(block).unwrap_or(now));
        let last_frame = frames.saturating_sub(1) as f64;
        while let Some(timed) = self.queue.pop() {
            let late = timed.at.saturating_duration_since(previous).as_secs_f64();
            let offset = (late * sample_rate).round().clamp(0.0, last_frame);
            handle(offset as u32, timed.message);
        }
        self.previous = Some(now);
    }
}

/// Lists the MIDI ports that can be sent to, each with the protocol it
/// would speak if opened preferring MIDI 2.0
/// ([`MidiOutput::open_with_protocol`]), and why not MIDI 2.0 where it is
/// MIDI 1.0. Nothing is sent.
///
/// The ports are [`midi_output_ports`]'s. Asking which protocol they speak
/// opens the system's UMP output for a moment: on Linux a sequencer client
/// is opened and closed.
pub fn midi_output_ports_with_protocols() -> Result<Vec<MidiOutputPortInfo>, MidiError> {
    Ok(ump::describe(&ump::native(), midi_output_ports()?))
}

/// How an open output port is reached.
enum Connection {
    /// Through `midir`, as MIDI 1.0 bytes.
    Bytes(midir::MidiOutputConnection),
    /// Through the system's own Universal MIDI Packet output.
    Ump {
        port: Box<dyn UmpPort>,
        /// Reads what [`MidiOutput::send_raw`] is given, to send it as
        /// packets.
        parser: StreamParser,
    },
}

/// An open MIDI output port.
///
/// Sending talks to the operating system and may block, so it does not
/// belong on the audio thread. Send from another thread, fed by a queue:
/// [`midi_output_queue`] makes one, whose audio end stamps each packet
/// with its frame, and [`spawn`](Self::spawn) starts the thread that sends
/// them on time.
///
/// A port speaks MIDI 1.0 unless it was opened with
/// [`open_with_protocol`](Self::open_with_protocol) preferring MIDI 2.0
/// and both the system and the port speak it ([`MidiProtocol`]). At MIDI
/// 1.0, Universal MIDI Packets are written out by the default
/// translation, and MIDI 2.0 that shapes a single note reaches the port
/// only if it was written as MPE first; at MIDI 2.0 they leave unchanged.
pub struct MidiOutput {
    name: String,
    connection: Connection,
    fallback: Option<ProtocolFallback>,
}

impl MidiOutput {
    /// Opens the output port with this identifier, as found in
    /// [`MidiPortInfo::id`]. The port speaks MIDI 1.0, the default; see
    /// [`open_with_protocol`](Self::open_with_protocol) for MIDI 2.0.
    pub fn open(id: &str) -> Result<Self, MidiError> {
        Self::open_with_protocol(id, MidiProtocol::Midi1)
    }

    /// Opens the output port with this identifier, speaking `preferred`
    /// where the system and the port can, and MIDI 1.0 otherwise.
    ///
    /// Asked for [`MidiProtocol::Midi2`], the port is opened through the
    /// system's own Universal MIDI Packet output when the system has one
    /// ([`ump_output_backend`]) and the port says it speaks MIDI 2.0; if
    /// either is missing, or that output fails to open, the port is opened
    /// as [`open`](Self::open) opens it, and [`fallback`](Self::fallback)
    /// says why. [`protocol`](Self::protocol) says which it is.
    ///
    /// ```no_run
    /// use manguebyte_devices::midi::{MidiOutput, MidiProtocol, midi_output_ports};
    ///
    /// let Some(port) = midi_output_ports()?.into_iter().next() else {
    ///     return Ok(());
    /// };
    /// let output = MidiOutput::open_with_protocol(&port.id, MidiProtocol::Midi2)?;
    /// match output.fallback() {
    ///     None => println!("{}: MIDI 2.0", output.name()),
    ///     Some(why) => println!("{}: MIDI 1.0, because {why}", output.name()),
    /// }
    /// # Ok::<(), manguebyte_devices::midi::MidiError>(())
    /// ```
    pub fn open_with_protocol(id: &str, preferred: MidiProtocol) -> Result<Self, MidiError> {
        let output = midir::MidiOutput::new(CLIENT_NAME).map_err(|_| MidiError::Unavailable)?;
        let (port, name) = output
            .ports()
            .into_iter()
            .map(|port| {
                let name = port_name(output.port_name(&port));
                (port, name)
            })
            .find(|(port, name)| port_id(&port.id(), name) == id)
            .ok_or_else(|| MidiError::PortNotFound(id.to_owned()))?;
        let fallback = match ump::select(&ump::native(), preferred, id) {
            Ok(port) => {
                return Ok(Self {
                    name,
                    connection: Connection::Ump {
                        port,
                        parser: StreamParser::new(SYSEX_OUT_CAPACITY),
                    },
                    fallback: None,
                });
            }
            Err(fallback) => fallback,
        };
        let connection = output
            .connect(&port, "manguebyte-out")
            .map_err(|error| MidiError::Connect(error.to_string()))?;
        Ok(Self {
            name,
            connection: Connection::Bytes(connection),
            fallback: Some(fallback),
        })
    }

    /// Creates a virtual output port called `name`, which other programs
    /// see as a MIDI input they can receive from. It speaks MIDI 1.0.
    ///
    /// Fails with [`MidiError::VirtualPortsUnsupported`] on Windows and
    /// Android; see the [module documentation](self#virtual-ports).
    pub fn create_virtual(name: &str) -> Result<Self, MidiError> {
        #[cfg(all(unix, not(target_os = "android")))]
        {
            use midir::os::unix::VirtualOutput;

            let output = midir::MidiOutput::new(CLIENT_NAME).map_err(|_| MidiError::Unavailable)?;
            let connection = output
                .create_virtual(name)
                .map_err(|error| MidiError::Connect(error.to_string()))?;
            Ok(Self {
                name: name.to_owned(),
                connection: Connection::Bytes(connection),
                fallback: Some(ProtocolFallback::NotRequested),
            })
        }
        #[cfg(not(all(unix, not(target_os = "android"))))]
        {
            let _ = name;
            Err(MidiError::VirtualPortsUnsupported)
        }
    }

    /// Human-readable name of the port.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Sends a channel message. A port that speaks MIDI 2.0 is sent it as
    /// a MIDI 1.0 channel voice packet on group 0.
    pub fn send(&mut self, message: MidiMessage) -> Result<(), MidiError> {
        self.send_raw(message.to_bytes().as_slice())
    }

    /// Sends a system message. A port that speaks MIDI 2.0 is sent it as
    /// a system packet on group 0.
    pub fn send_system(&mut self, message: SystemMessage) -> Result<(), MidiError> {
        let mut bytes = [0; 3];
        let len = message.write(&mut bytes);
        self.send_raw(&bytes[..len])
    }

    /// Sends raw bytes: one complete message, which may be system
    /// exclusive.
    ///
    /// A port that speaks MIDI 2.0 is sent the packets the bytes stand
    /// for, on group 0: a channel or system message as one packet, system
    /// exclusive as 7-bit system exclusive packets, as the MIDI-CI replies
    /// of [`MidiInput::open_with_sysex`] need. System exclusive longer
    /// than [`SYSEX_OUT_CAPACITY`] is dropped there.
    pub fn send_raw(&mut self, bytes: &[u8]) -> Result<(), MidiError> {
        let (port, parser) = match &mut self.connection {
            Connection::Bytes(connection) => {
                return connection
                    .send(bytes)
                    .map_err(|error| MidiError::Send(error.to_string()));
            }
            Connection::Ump { port, parser } => (port, parser),
        };
        let mut result = Ok(());
        let mut send = |packet: Packet| {
            if result.is_ok() {
                result = port.send(packet.words());
            }
        };
        parser.feed_slice(bytes, |event| match event {
            StreamEvent::Channel(message) => send(Packet::from_midi1(0, message)),
            StreamEvent::System(message) => {
                send(UmpMessage::System { group: 0, message }.to_packet())
            }
            StreamEvent::SysEx(payload) => {
                // The parser puts only data bytes, below 0x80, in a
                // payload, so cutting it into packets cannot fail.
                if let Ok(packets) = sysex7_packets(0, payload) {
                    packets.for_each(&mut send);
                }
            }
        });
        // A message cut short is not carried over to the next call.
        parser.reset();
        result?;
        port.flush()
    }

    /// [`MidiSender::send_ump`](crate::backend::MidiSender::send_ump).
    pub(crate) fn send_ump_words(&mut self, words: &[u32]) -> Result<(), MidiError> {
        match &mut self.connection {
            Connection::Ump { port, .. } => port.send(words),
            Connection::Bytes(_) => Err(MidiError::Send(String::from("the port speaks MIDI 1.0"))),
        }
    }

    /// [`MidiSender::flush`](crate::backend::MidiSender::flush).
    pub(crate) fn flush_ump(&mut self) -> Result<(), MidiError> {
        match &mut self.connection {
            Connection::Ump { port, .. } => port.flush(),
            Connection::Bytes(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;
    const FRAMES: usize = 480; // 10 ms

    fn note(key: u8) -> MidiInputMessage {
        MidiInputMessage::Channel(MidiMessage::NoteOn {
            channel: 0,
            note: key,
            velocity: 100,
        })
    }

    fn read(receiver: &mut MidiReceiver, now: Instant) -> Vec<(u32, MidiInputMessage)> {
        let mut seen = Vec::new();
        receiver.read_at(now, FRAMES, RATE, |offset, message| {
            seen.push((offset, message))
        });
        seen
    }

    fn ms(milliseconds: f64) -> Duration {
        Duration::from_secs_f64(milliseconds / 1_000.0)
    }

    #[test]
    fn a_message_lands_where_it_fell_in_the_previous_block() {
        let (mut sender, mut receiver) = receiver_pair(16);
        let start = Instant::now();
        assert!(read(&mut receiver, start).is_empty());

        let push = |sender: &mut Producer<Timed>, at: f64, key: u8| {
            sender
                .push(Timed {
                    at: start + ms(at),
                    message: note(key),
                })
                .unwrap();
        };
        push(&mut sender, 2.5, 60);
        push(&mut sender, 7.5, 64);
        assert_eq!(
            read(&mut receiver, start + ms(10.0)),
            [(120, note(60)), (360, note(64))]
        );

        // The distance between two notes is kept across a block boundary:
        // 7.5 ms and 12.5 ms are 5 ms apart, which is 240 frames.
        push(&mut sender, 12.5, 67);
        assert_eq!(read(&mut receiver, start + ms(20.0)), [(120, note(67))]);
    }

    #[test]
    fn late_callbacks_and_the_first_block_are_handled() {
        let (mut sender, mut receiver) = receiver_pair(16);
        let start = Instant::now();

        // Before any block has been read, a message is placed as if the
        // previous block had just ended.
        sender
            .push(Timed {
                at: start + ms(5.0),
                message: note(1),
            })
            .unwrap();
        assert_eq!(read(&mut receiver, start + ms(10.0)), [(240, note(1))]);

        // The audio callback came 30 ms later instead of 10. Messages from
        // the first 10 ms keep their place; later ones are held at the last
        // frame rather than spilling out of the block.
        for (at, key) in [(12.0, 2), (25.0, 3), (39.0, 4)] {
            sender
                .push(Timed {
                    at: start + ms(at),
                    message: note(key),
                })
                .unwrap();
        }
        assert_eq!(
            read(&mut receiver, start + ms(40.0)),
            [(96, note(2)), (479, note(3)), (479, note(4))]
        );

        // A message stamped before the previous block was read (it was
        // pushed while that read was under way) goes at the start.
        sender
            .push(Timed {
                at: start + ms(39.5),
                message: note(5),
            })
            .unwrap();
        assert_eq!(read(&mut receiver, start + ms(50.0)), [(0, note(5))]);
    }

    #[test]
    fn a_port_without_a_system_id_is_identified_by_its_name() {
        assert_eq!(port_id("usb-1234", "Keystation"), "usb-1234");
        assert_eq!(
            port_id("", "Microsoft GS Wavetable Synth"),
            "name:Microsoft GS Wavetable Synth"
        );
    }

    #[test]
    fn a_shared_sink_merges_ports_into_one_receiver() {
        let (shared, mut receiver) = merged_receiver();
        let mut first = MidiSink::Shared(Arc::clone(&shared));
        let mut second = MidiSink::Shared(shared);
        let (mut a, mut b) = (StreamParser::new(0), StreamParser::new(0));
        // A message split across two deliveries and running status stay
        // with their own port, because each port has its own parser.
        first.deliver(&[0x90, 60], &mut a);
        second.deliver(&[0x90, 64, 100], &mut b);
        first.deliver(&[100, 62, 90], &mut a);
        second.deliver(&[0xF0, 1, 2, 0xF7], &mut b);
        let mut seen = Vec::new();
        receiver.read(FRAMES, RATE, |_, message| seen.push(message));
        let on = |note, velocity| {
            MidiInputMessage::Channel(MidiMessage::NoteOn {
                channel: 0,
                note,
                velocity,
            })
        };
        assert_eq!(seen, [on(64, 100), on(60, 100), on(62, 90)]);
    }

    #[test]
    fn system_exclusive_messages_reach_their_handler_and_the_rest_the_queue() {
        use manguebyte_midi::ci::Muid;
        use manguebyte_midi::ci::agent::{Agent, AgentConfig, AgentEvent};

        let (sender, mut receiver) = receiver_pair(16);
        let mut sink = MidiSink::Own(sender);
        let mut parser = StreamParser::new(SYSEX_CAPACITY);
        // A MIDI-CI discovery from another device, cut into two
        // deliveries, then a note.
        let mut other = Agent::new(Muid::from_seed(7), AgentConfig::default());
        other.discover();
        let discovery = other.outgoing().unwrap();
        let (head, tail) = discovery.split_at(10);
        let mut payloads = Vec::new();
        let mut handle = |payload: &[u8]| payloads.push(payload.to_vec());
        sink.deliver_with_sysex(head, &mut parser, &mut handle);
        sink.deliver_with_sysex(tail, &mut parser, &mut handle);
        sink.deliver_with_sysex(&[0x90, 60, 100], &mut parser, &mut handle);
        let mut seen = Vec::new();
        receiver.read(FRAMES, RATE, |_, message| seen.push(message));
        assert_eq!(seen, [note(60)]);
        assert_eq!(payloads.len(), 1);
        let mut agent = Agent::new(Muid::from_seed(8), AgentConfig::default());
        let events = agent.receive(&payloads[0]).unwrap();
        assert!(
            matches!(events[..], [AgentEvent::PeerDiscovered(peer)] if peer.muid == other.muid())
        );
    }

    #[cfg(any(windows, target_os = "android"))]
    #[test]
    fn virtual_ports_are_refused_where_the_system_has_none() {
        assert!(matches!(
            MidiInput::create_virtual("Manguebyte test"),
            Err(MidiError::VirtualPortsUnsupported)
        ));
        assert!(matches!(
            MidiOutput::create_virtual("Manguebyte test"),
            Err(MidiError::VirtualPortsUnsupported)
        ));
    }

    #[test]
    fn offsets_never_run_backwards() {
        let (mut sender, mut receiver) = receiver_pair(64);
        let start = Instant::now();
        read(&mut receiver, start);
        for step in 0..40u32 {
            sender
                .push(Timed {
                    at: start + ms(f64::from(step) * 0.3),
                    message: MidiInputMessage::System(SystemMessage::TimingClock),
                })
                .unwrap();
        }
        let seen = read(&mut receiver, start + ms(10.0));
        assert_eq!(seen.len(), 40);
        assert!(seen.windows(2).all(|pair| pair[0].0 <= pair[1].0));
    }
}
