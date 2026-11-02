//! The device manager's MIDI output port, shared by the program's thread,
//! which opens, changes and closes it, and the thread that sends it what
//! the audio thread queues.
//!
//! The audio thread never reaches it: it pushes packets into a
//! [`MidiOutputQueue`](super::MidiOutputQueue), which is wait-free, and
//! the sending thread takes them from there and locks the port to send
//! them. A change of port waits, on the program's thread, at most for the
//! packets being sent at that moment.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use manguebyte_midi::MidiMessage;
use manguebyte_midi::ump::{ChannelVoice2, Message, NoteAttribute, Packet};

use super::{MidiError, MidiOutputQueue, MidiProtocol, PacketWriter};
use crate::backend::MidiSender;

/// How often the sending thread looks for packets when it has none
/// waiting, and the longest it sleeps before one is due.
const POLL: Duration = Duration::from_millis(1);

/// What a message does to the notes a port plays.
enum NoteChange {
    On {
        group: u8,
        channel: u8,
        key: u8,
    },
    Off {
        group: u8,
        channel: u8,
        key: u8,
    },
    /// The sustain pedal (controller 64) went down or up.
    Pedal {
        group: u8,
        channel: u8,
        down: bool,
    },
    Other,
}

/// A note, as (group, channel, key).
type NoteAt = (u8, u8, u8);

/// A channel, as (group, channel).
type ChannelAt = (u8, u8);

/// The sustain pedal's controller.
const SUSTAIN: u8 = 64;

impl NoteChange {
    fn of_midi1(group: u8, message: &MidiMessage) -> Self {
        match *message {
            // A note-on of velocity zero is a note-off in MIDI 1.0.
            MidiMessage::NoteOn {
                channel,
                note,
                velocity,
            } if velocity > 0 => Self::On {
                group,
                channel,
                key: note,
            },
            MidiMessage::NoteOn { channel, note, .. }
            | MidiMessage::NoteOff { channel, note, .. } => Self::Off {
                group,
                channel,
                key: note,
            },
            MidiMessage::ControlChange {
                channel,
                controller: SUSTAIN,
                value,
            } => Self::Pedal {
                group,
                channel,
                down: value >= 64,
            },
            _ => Self::Other,
        }
    }

    fn of_packet(packet: &Packet) -> Self {
        match packet.message() {
            Ok(Message::Midi1 { group, message }) => Self::of_midi1(group, &message),
            Ok(Message::Midi2 {
                group,
                message: ChannelVoice2::NoteOn { channel, note, .. },
            }) => Self::On {
                group,
                channel,
                key: note,
            },
            Ok(Message::Midi2 {
                group,
                message: ChannelVoice2::NoteOff { channel, note, .. },
            }) => Self::Off {
                group,
                channel,
                key: note,
            },
            Ok(Message::Midi2 {
                group,
                message:
                    ChannelVoice2::ControlChange {
                        channel,
                        controller: SUSTAIN,
                        value,
                    },
            }) => Self::Pedal {
                group,
                channel,
                down: value >= 0x8000_0000,
            },
            _ => Self::Other,
        }
    }
}

/// The notes a port is playing, one bit per note number for each group
/// and channel, and the channels whose sustain pedal is down, which would
/// keep them sounding after their note-offs.
struct HeldNotes {
    keys: [[u128; 16]; 16],
    pedals: [u16; 16],
}

impl HeldNotes {
    const NONE: Self = Self {
        keys: [[0; 16]; 16],
        pedals: [0; 16],
    };

    /// Follows a message on its way to the port, and says whether it is
    /// to be sent: every message is, but a note-off for a note the port is
    /// not playing, such as one that a change of port has already ended.
    fn follow(&mut self, change: NoteChange) -> bool {
        match change {
            NoteChange::On {
                group,
                channel,
                key,
            } => {
                self.keys[usize::from(group & 0x0F)][usize::from(channel & 0x0F)] |=
                    1 << (key & 0x7F);
                true
            }
            NoteChange::Off {
                group,
                channel,
                key,
            } => {
                let keys = &mut self.keys[usize::from(group & 0x0F)][usize::from(channel & 0x0F)];
                let bit = 1 << (key & 0x7F);
                let held = *keys & bit != 0;
                *keys &= !bit;
                held
            }
            NoteChange::Pedal {
                group,
                channel,
                down,
            } => {
                let bit = 1 << (channel & 0x0F);
                let pedals = &mut self.pedals[usize::from(group & 0x0F)];
                if down {
                    *pedals |= bit;
                } else {
                    *pedals &= !bit;
                }
                true
            }
            NoteChange::Other => true,
        }
    }

    /// Takes every note held, as (group, channel, key), and every channel
    /// whose pedal is down, as (group, channel).
    fn drain(&mut self) -> (Vec<NoteAt>, Vec<ChannelAt>) {
        let Self { keys, pedals } = std::mem::replace(self, Self::NONE);
        let mut notes = Vec::new();
        let mut held_down = Vec::new();
        for ((group, channels), pedals) in (0..16u8).zip(keys).zip(pedals) {
            for (channel, keys) in (0..16u8).zip(channels) {
                for key in (0..128u8).filter(|&key| keys & (1u128 << key) != 0) {
                    notes.push((group, channel, key));
                }
                if pedals & (1 << channel) != 0 {
                    held_down.push((group, channel));
                }
            }
        }
        (notes, held_down)
    }
}

/// An open port with what sending to it needs.
struct Open {
    sender: Box<dyn MidiSender>,
    protocol: MidiProtocol,
    writer: PacketWriter,
    held: HeldNotes,
    /// Whether packets were handed to a MIDI 2.0 port since the last
    /// flush.
    pending: bool,
}

impl Open {
    fn send_packet(&mut self, packet: &Packet) -> Result<(), MidiError> {
        if !self.held.follow(NoteChange::of_packet(packet)) {
            return Ok(());
        }
        if self.protocol == MidiProtocol::Midi2 {
            self.pending = true;
            return self.sender.send_ump(packet.words());
        }
        let Self { sender, writer, .. } = self;
        let mut result = Ok(());
        writer.write(packet, |bytes| {
            if result.is_ok() {
                result = sender.send(bytes);
            }
        });
        result
    }

    fn flush(&mut self) -> Result<(), MidiError> {
        if !std::mem::take(&mut self.pending) {
            return Ok(());
        }
        self.sender.flush()
    }

    /// Ends every note the port plays, with a note-off of velocity zero
    /// each, then lifts every sustain pedal held down, in the port's
    /// protocol. Returns how many notes.
    fn release(&mut self) -> usize {
        let (notes, pedals) = self.held.drain();
        let offs = notes.iter().map(|&(group, channel, key)| {
            let off = ChannelVoice2::NoteOff {
                channel,
                note: key,
                velocity: 0,
                attribute: NoteAttribute::NONE,
            };
            (group, off, [0x80 | channel, key, 0])
        });
        let lifts = pedals.iter().map(|&(group, channel)| {
            let up = ChannelVoice2::ControlChange {
                channel,
                controller: SUSTAIN,
                value: 0,
            };
            (group, up, [0xB0 | channel, SUSTAIN, 0])
        });
        for (group, message, bytes) in offs.chain(lifts) {
            // A port that refuses them, such as one unplugged, cannot be
            // helped.
            let _ = match self.protocol {
                MidiProtocol::Midi2 => {
                    self.pending = true;
                    let packet = Message::Midi2 { group, message }.to_packet();
                    self.sender.send_ump(packet.words())
                }
                _ => self.sender.send(&bytes),
            };
        }
        let _ = self.flush();
        notes.len()
    }
}

/// The output port of a device manager, or none. See the module
/// documentation.
pub(crate) struct SharedOutput {
    open: Option<Open>,
    /// Messages the ports refused, counted, since the sending thread has
    /// nobody to tell.
    errors: u64,
    /// Notes ended because their port was changed or closed.
    released: u64,
}

impl SharedOutput {
    pub(crate) fn new() -> Self {
        Self {
            open: None,
            errors: 0,
            released: 0,
        }
    }

    /// Locks a shared output, whatever a panic on another thread left.
    pub(crate) fn lock(shared: &Mutex<Self>) -> MutexGuard<'_, Self> {
        shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The protocol the open port speaks.
    pub(crate) fn protocol(&self) -> Option<MidiProtocol> {
        self.open.as_ref().map(|open| open.protocol)
    }

    /// Puts `next` in place of the open port, or closes it with `None`.
    /// The old port is first sent a note-off for every note it plays, and
    /// its sustain pedals are lifted.
    pub(crate) fn replace(&mut self, next: Option<Box<dyn MidiSender>>) {
        if let Some(mut old) = self.open.take() {
            self.released += old.release() as u64;
        }
        self.open = next.map(|sender| Open {
            protocol: sender.protocol(),
            sender,
            writer: PacketWriter::new(),
            held: HeldNotes::NONE,
            pending: false,
        });
    }

    /// Sends one complete MIDI 1.0 message, from the program's thread.
    pub(crate) fn send_bytes(&mut self, bytes: &[u8]) -> Result<(), MidiError> {
        let open = self.open.as_mut().ok_or_else(not_open)?;
        if let Some(message) = MidiMessage::from_bytes(bytes) {
            if !open.held.follow(NoteChange::of_midi1(0, &message)) {
                return Ok(());
            }
        }
        open.sender.send(bytes)
    }

    /// Sends one packet, from the program's thread.
    pub(crate) fn send_packet(&mut self, packet: &Packet) -> Result<(), MidiError> {
        let open = self.open.as_mut().ok_or_else(not_open)?;
        open.send_packet(packet)?;
        open.flush()
    }

    /// Sends one packet the audio thread queued, to the port open now; a
    /// packet due while no port is open is dropped. A refusal is counted.
    fn send_queued(&mut self, packet: &Packet) {
        if let Some(open) = &mut self.open {
            if open.send_packet(packet).is_err() {
                self.errors += 1;
            }
        }
    }

    fn flush_queued(&mut self) {
        if let Some(open) = &mut self.open {
            if open.flush().is_err() {
                self.errors += 1;
            }
        }
    }

    /// How many messages queued by the audio thread the ports refused.
    pub(crate) fn errors(&self) -> u64 {
        self.errors
    }

    /// How many notes were ended because their port changed or closed.
    pub(crate) fn released(&self) -> u64 {
        self.released
    }
}

fn not_open() -> MidiError {
    MidiError::PortNotFound(String::from("no MIDI output is open"))
}

/// A thread that sends whatever port the [`SharedOutput`] has open what
/// the audio thread queued, each packet when it is due. Dropping it sends
/// what is still waiting and stops the thread; the port stays open.
pub(crate) struct SharedOutputThread {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SharedOutputThread {
    pub(crate) fn start(shared: Arc<Mutex<SharedOutput>>, mut queue: MidiOutputQueue) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name(String::from("manguebyte-midi-out"))
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let now = Instant::now();
                        let next = {
                            let mut output = SharedOutput::lock(&shared);
                            let next = queue.send_due(now, |packet| output.send_queued(packet));
                            output.flush_queued();
                            next
                        };
                        let wait = match next {
                            Some(due) => due.saturating_duration_since(now).min(POLL),
                            None => POLL,
                        };
                        std::thread::sleep(wait);
                    }
                    let mut output = SharedOutput::lock(&shared);
                    queue.send_all(|packet| output.send_queued(packet));
                    output.flush_queued();
                })
                .ok()
        };
        Self { stop, thread }
    }

    /// Whether the thread runs. It does not if the system refused to start
    /// one.
    pub(crate) fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }
}

impl Drop for SharedOutputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
