//! Universal MIDI Packets sent to output ports, on time, from the audio
//! thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use manguebyte_core::spsc::{self, Consumer, Producer};
use manguebyte_midi::translate::midi2_to_midi1;
use manguebyte_midi::ump::{Assembler, Message, Packet};

use super::ump::ProtocolFallback;
use super::{Connection, MidiError, MidiOutput};
#[cfg(doc)]
use super::{midi_output_ports_with_protocols, ump_output_backend};
use crate::backend::MidiSender;

/// The largest system exclusive message a [`PacketWriter`] puts together
/// from 7-bit system exclusive packets, in bytes, without its `0xF0` and
/// `0xF7`.
pub const SYSEX_OUT_CAPACITY: usize = 65_536;

/// How often the sending thread looks for packets when it has none
/// waiting, and the longest it sleeps before one is due.
const POLL: Duration = Duration::from_millis(1);

/// The protocol an output port speaks.
///
/// A port speaks MIDI 1.0 unless an application asks for MIDI 2.0
/// ([`MidiOutput::open_with_protocol`]) and both the system and the port
/// speak it:
///
/// - **MIDI 1.0** goes through `midir`, as bytes: WinMM on Windows,
///   CoreMIDI's MIDI 1.0 calls on macOS and iOS, the ALSA sequencer's MIDI
///   1.0 events on Linux. Packets are written out by the specification's
///   default translation ([`PacketWriter`]).
/// - **MIDI 2.0** goes through the system's own Universal MIDI Packet
///   output, and every packet leaves as it is, without translation:
///   CoreMIDI's `MIDISendEventList` in `kMIDIProtocol_2_0` (macOS 11, iOS
///   14), the ALSA sequencer's UMP events from a MIDI 2.0 client (alsa-lib
///   1.2.10, Linux 6.5). Windows has none that this crate can reach
///   ([`ump_output_backend`] says why).
///
/// [`MidiOutput::protocol`] says which one an open port speaks, and
/// [`MidiOutput::fallback`] why it is MIDI 1.0; [`midi_output_ports_with_protocols`]
/// says it for every port before any is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MidiProtocol {
    /// MIDI 1.0 bytes.
    Midi1,
    /// Universal MIDI Packets in the MIDI 2.0 protocol, sent unchanged
    /// through the system's own UMP output.
    ///
    /// Packets of every message type go out as they are: MIDI 2.0 channel
    /// voice with its per-note controllers and attributes, system
    /// messages, system exclusive, and MIDI 1.0 channel voice packets
    /// (message type 2) too. A receiver in the MIDI 2.0 protocol expects
    /// message type 4 for channel voice; turn MIDI 1.0 into it first with
    /// [`UmpTranslator::to_midi2`](manguebyte_midi::translate::UmpTranslator::to_midi2)
    /// where that matters.
    Midi2,
}

/// Writes Universal MIDI Packets as the MIDI 1.0 bytes a port takes.
///
/// Used for ports that speak [`MidiProtocol::Midi1`]; a port that speaks
/// [`MidiProtocol::Midi2`] takes the packets as they are.
///
/// - MIDI 1.0 channel voice packets (message type 2) and system common and
///   real-time packets (type 1) become their bytes.
/// - MIDI 2.0 channel voice packets (type 4) become the MIDI 1.0 messages
///   of the specification's default translation
///   ([`midi2_to_midi1`]), which drops what has no MIDI 1.0 form, such as
///   per-note controllers (M2-104-UM, section D.2.8). Write MPE first, with
///   [`Midi2ToMpe`](manguebyte_midi::translate::Midi2ToMpe), for a receiver
///   that should keep them.
/// - 7-bit system exclusive packets (type 3) are put together and sent as
///   one message, `0xF0` to `0xF7`, when the last arrives.
/// - Everything else (utility, 8-bit system exclusive, mixed data, flex
///   data and stream messages) has no MIDI 1.0 form and is dropped.
///
/// After [`new`](Self::new), which allocates, nothing allocates.
#[derive(Debug)]
pub struct PacketWriter {
    sysex: Assembler,
    /// Room for a whole system exclusive message with its two framing
    /// bytes.
    framed: Vec<u8>,
}

impl PacketWriter {
    /// A writer that puts together system exclusive messages of up to
    /// [`SYSEX_OUT_CAPACITY`] bytes. Allocates.
    pub fn new() -> Self {
        Self {
            sysex: Assembler::new(SYSEX_OUT_CAPACITY),
            framed: Vec::with_capacity(SYSEX_OUT_CAPACITY + 2),
        }
    }

    /// Hands each complete MIDI 1.0 message `packet` stands for to `send`,
    /// in order.
    pub fn write(&mut self, packet: &Packet, mut send: impl FnMut(&[u8])) {
        match packet.message() {
            Ok(Message::Midi1 { message, .. }) => send(message.to_bytes().as_slice()),
            Ok(Message::Midi2 { message, .. }) => {
                for translated in midi2_to_midi1(message).as_slice() {
                    send(translated.to_bytes().as_slice());
                }
            }
            Ok(Message::System { message, .. }) => {
                let mut bytes = [0; 3];
                let len = message.write(&mut bytes);
                send(&bytes[..len]);
            }
            Ok(Message::SysEx7(piece)) => {
                if let Some(whole) = self.sysex.push(piece.part, piece.data()) {
                    self.framed.clear();
                    self.framed.push(0xF0);
                    self.framed.extend_from_slice(whole);
                    self.framed.push(0xF7);
                    send(&self.framed);
                }
            }
            _ => {}
        }
    }
}

impl Default for PacketWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiOutput {
    /// The protocol the port speaks. See [`MidiProtocol`].
    pub fn protocol(&self) -> MidiProtocol {
        match self.connection {
            Connection::Bytes(_) => MidiProtocol::Midi1,
            Connection::Ump { .. } => MidiProtocol::Midi2,
        }
    }

    /// Why the port speaks MIDI 1.0, or `None` if it speaks MIDI 2.0.
    pub fn fallback(&self) -> Option<&ProtocolFallback> {
        self.fallback.as_ref()
    }

    /// Sends a packet. A port that speaks MIDI 2.0 is sent it unchanged; a
    /// port that speaks MIDI 1.0 is sent the bytes it stands for, through
    /// `writer`, which keeps what a system exclusive message needs between
    /// packets (see [`PacketWriter`] for what becomes what).
    ///
    /// This talks to the operating system and may block: never call it
    /// from the audio thread. There, use [`midi_output_queue`] and
    /// [`spawn`](Self::spawn).
    pub fn send_packet(
        &mut self,
        writer: &mut PacketWriter,
        packet: &Packet,
    ) -> Result<(), MidiError> {
        if let Connection::Ump { port, .. } = &mut self.connection {
            port.send(packet.words())?;
            return port.flush();
        }
        let mut result = Ok(());
        writer.write(packet, |bytes| {
            if result.is_ok() {
                result = self.send_raw(bytes);
            }
        });
        result
    }

    /// Starts a thread that sends this port what `queue` carries, each
    /// packet when it is due. The port is closed when the returned
    /// [`MidiOutputThread`] is dropped.
    pub fn spawn(self, queue: MidiOutputQueue) -> MidiOutputThread {
        MidiOutputThread::start(Box::new(self), queue)
    }
}

/// A packet and the moment it is to be sent.
#[derive(Debug, Clone, Copy)]
struct TimedPacket {
    at: Instant,
    packet: Packet,
}

/// Makes the two ends of a queue of packets from the audio thread to an
/// output port, with room for `capacity` packets waiting.
///
/// The audio thread's end, the [`MidiTransmitter`], stamps each packet
/// with the moment its frame is heard; the other end, given to
/// [`MidiOutput::spawn`], sends it then. Allocates.
///
/// ```
/// use std::time::{Duration, Instant};
/// use manguebyte_devices::midi::midi_output_queue;
/// use manguebyte_midi::MidiMessage;
/// use manguebyte_midi::ump::Packet;
///
/// let (mut transmitter, mut queue) = midi_output_queue(64);
/// // In the audio callback, once per block, then for each packet:
/// transmitter.begin_block(480, 48_000.0);
/// let note = MidiMessage::NoteOn { channel: 0, note: 60, velocity: 100 };
/// assert!(transmitter.send(240, Packet::from_midi1(0, note)));
///
/// // What the sending thread does: nothing is due yet...
/// let mut sent = Vec::new();
/// queue.send_due(Instant::now(), |packet| sent.push(*packet));
/// assert!(sent.is_empty());
/// // ...until a block and a half from now.
/// queue.send_due(Instant::now() + Duration::from_millis(20), |packet| sent.push(*packet));
/// assert_eq!(sent.len(), 1);
/// ```
pub fn midi_output_queue(capacity: usize) -> (MidiTransmitter, MidiOutputQueue) {
    let (producer, consumer) = spsc::channel(capacity.max(1));
    let dropped = Arc::new(AtomicU64::new(0));
    (
        MidiTransmitter {
            queue: producer,
            block_start: None,
            sample_rate: 48_000.0,
            block: Duration::ZERO,
            dropped: Arc::clone(&dropped),
        },
        MidiOutputQueue {
            queue: consumer,
            waiting: None,
            dropped,
        },
    )
}

/// The audio thread's end of a MIDI output: it queues packets with the
/// frame of the block they belong to.
///
/// # Timing
///
/// As with input (see the [module documentation](super#timing)), every
/// packet is late by one block, and by the same amount: a packet on frame
/// `n` of a block is sent one block after the block began, plus `n`
/// frames. The time the block began is taken when
/// [`begin_block`](Self::begin_block) is called, which is why it belongs
/// at the very start of the audio callback. How close to that moment the
/// bytes leave depends on the sending thread's sleep, which the operating
/// system rounds to its timer: about a millisecond on macOS and Linux, and
/// on Windows as much as 15.6 ms unless something has raised the timer's
/// resolution.
pub struct MidiTransmitter {
    queue: Producer<TimedPacket>,
    block_start: Option<Instant>,
    sample_rate: f64,
    block: Duration,
    dropped: Arc<AtomicU64>,
}

impl MidiTransmitter {
    /// Starts a block of `frames` frames at `sample_rate`. Call it once per
    /// block, at the start of the audio callback, before
    /// [`send`](Self::send). Real-time safe: it reads the clock.
    pub fn begin_block(&mut self, frames: usize, sample_rate: f64) {
        self.begin_block_at(Instant::now(), frames, sample_rate);
    }

    fn begin_block_at(&mut self, now: Instant, frames: usize, sample_rate: f64) {
        self.sample_rate = sample_rate.max(1.0);
        self.block = Duration::from_secs_f64(frames as f64 / self.sample_rate);
        self.block_start = Some(now);
    }

    /// Queues `packet` to be sent at frame `offset` of the block. Returns
    /// `false`, and counts it as dropped, if the queue is full. Real-time
    /// safe.
    pub fn send(&mut self, offset: u32, packet: Packet) -> bool {
        let start = self.block_start.unwrap_or_else(Instant::now);
        let at = start + self.block + Duration::from_secs_f64(f64::from(offset) / self.sample_rate);
        if self.queue.push(TimedPacket { at, packet }).is_ok() {
            true
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            false
        }
    }

    /// How many packets have been dropped because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// The sending end of a MIDI output queue: what [`MidiOutput::spawn`]
/// sends from, or what an application with its own sending thread reads.
pub struct MidiOutputQueue {
    queue: Consumer<TimedPacket>,
    /// The first packet not yet due, taken off the queue to look at.
    waiting: Option<TimedPacket>,
    dropped: Arc<AtomicU64>,
}

impl MidiOutputQueue {
    /// Hands every packet due by `now` to `send`, in the order they were
    /// queued, and returns when the next one waiting is due, if one is.
    ///
    /// Packets keep their order: one stamped earlier than a packet queued
    /// before it, as a late audio callback can make it, goes right after
    /// that packet.
    pub fn send_due(&mut self, now: Instant, mut send: impl FnMut(&Packet)) -> Option<Instant> {
        loop {
            let next = match self.waiting.take() {
                Some(waiting) => waiting,
                None => self.queue.pop()?,
            };
            if next.at > now {
                let at = next.at;
                self.waiting = Some(next);
                return Some(at);
            }
            send(&next.packet);
        }
    }

    /// Hands every packet waiting to `send` at once, due or not.
    pub fn send_all(&mut self, mut send: impl FnMut(&Packet)) {
        if let Some(waiting) = self.waiting.take() {
            send(&waiting.packet);
        }
        while let Some(next) = self.queue.pop() {
            send(&next.packet);
        }
    }

    /// How many packets the transmitter has dropped because the queue was
    /// full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// What the sending thread does with each due packet, by the port's
/// protocol: unchanged to a MIDI 2.0 port, gathered until
/// [`flush`](Self::flush) so that packets due together reach the system in
/// one call (one CoreMIDI event list); as MIDI 1.0 bytes to the others.
struct Sending<'a> {
    port: &'a mut dyn MidiSender,
    ump: bool,
    /// Whether packets were handed to a MIDI 2.0 port since the last
    /// flush.
    pending: bool,
    writer: PacketWriter,
    errors: &'a AtomicU64,
}

impl<'a> Sending<'a> {
    fn new(port: &'a mut dyn MidiSender, errors: &'a AtomicU64) -> Self {
        Self {
            ump: port.protocol() == MidiProtocol::Midi2,
            port,
            pending: false,
            writer: PacketWriter::new(),
            errors,
        }
    }

    fn count(&self, result: Result<(), MidiError>) {
        if result.is_err() {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn packet(&mut self, packet: &Packet) {
        if self.ump {
            let result = self.port.send_ump(packet.words());
            self.count(result);
            self.pending = true;
            return;
        }
        let Self {
            port,
            writer,
            errors,
            ..
        } = self;
        writer.write(packet, |bytes| {
            if port.send(bytes).is_err() {
                errors.fetch_add(1, Ordering::Relaxed);
            }
        });
    }

    fn flush(&mut self) {
        if self.pending {
            self.pending = false;
            let result = self.port.flush();
            self.count(result);
        }
    }
}

/// A thread that sends an output port its packets on time. Dropping it
/// sends what is still waiting, stops the thread and closes the port.
///
/// A port that speaks MIDI 2.0 is handed every packet unchanged, and the
/// packets due at one wake-up go to the system together; a port that
/// speaks MIDI 1.0 is sent the bytes of [`PacketWriter`].
pub struct MidiOutputThread {
    stop: Arc<AtomicBool>,
    /// Send errors, counted, since a sending thread has nobody to tell.
    errors: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl MidiOutputThread {
    fn start(mut port: Box<dyn MidiSender>, mut queue: MidiOutputQueue) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let errors = Arc::new(AtomicU64::new(0));
        let thread = {
            let (stop, errors) = (Arc::clone(&stop), Arc::clone(&errors));
            std::thread::Builder::new()
                .name(String::from("manguebyte-midi-out"))
                .spawn(move || {
                    let mut sending = Sending::new(port.as_mut(), &errors);
                    while !stop.load(Ordering::Acquire) {
                        let now = Instant::now();
                        let next = queue.send_due(now, |packet| sending.packet(packet));
                        sending.flush();
                        let wait = match next {
                            Some(due) => due.saturating_duration_since(now).min(POLL),
                            None => POLL,
                        };
                        std::thread::sleep(wait);
                    }
                    queue.send_all(|packet| sending.packet(packet));
                    sending.flush();
                })
                .ok()
        };
        Self {
            stop,
            errors,
            thread,
        }
    }

    /// How many messages the port refused to send.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Whether the thread is running. It is not if the system refused to
    /// start one.
    pub fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }
}

impl Drop for MidiOutputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use manguebyte_midi::MidiMessage;
    use manguebyte_midi::ump::{ChannelVoice2, NoteAttribute, Part, SysEx7, sysex7_packets};

    use super::super::ump::tests::{FakePort, Sent};
    use super::*;

    fn written(packets: &[Packet]) -> Vec<Vec<u8>> {
        let mut writer = PacketWriter::new();
        let mut out = Vec::new();
        for packet in packets {
            writer.write(packet, |bytes| out.push(bytes.to_vec()));
        }
        out
    }

    #[test]
    fn packets_become_the_bytes_of_the_default_translation() {
        let note = |velocity| {
            Message::Midi2 {
                group: 3,
                message: ChannelVoice2::NoteOn {
                    channel: 1,
                    note: 60,
                    velocity,
                    attribute: NoteAttribute::NONE,
                },
            }
            .to_packet()
        };
        let bend = Message::Midi2 {
            group: 0,
            message: ChannelVoice2::PerNotePitchBend {
                channel: 1,
                note: 60,
                value: 0,
            },
        }
        .to_packet();
        let clock = Message::System {
            group: 0,
            message: manguebyte_midi::SystemMessage::TimingClock,
        }
        .to_packet();
        let cc = Packet::from_midi1(
            0,
            MidiMessage::ControlChange {
                channel: 2,
                controller: 7,
                value: 100,
            },
        );
        assert_eq!(
            written(&[note(0xAEBA), note(0), bend, clock, cc]),
            [
                // M2-104-UM, D.1.4: 0xAEBA is 0x57 in 7 bits.
                vec![0x91, 60, 0x57],
                // D.2.1: never velocity zero, which would be a note-off.
                vec![0x91, 60, 1],
                // D.2.8: per-note pitch bend has no MIDI 1.0 form.
                vec![0xF8],
                vec![0xB2, 7, 100],
            ]
        );
    }

    #[test]
    fn system_exclusive_packets_are_sent_as_one_message() {
        let payload: Vec<u8> = (0..20).collect();
        let packets: Vec<Packet> = sysex7_packets(0, &payload).unwrap().collect();
        assert!(packets.len() > 1);
        let mut expected = vec![0xF0];
        expected.extend_from_slice(&payload);
        expected.push(0xF7);
        assert_eq!(written(&packets), [expected]);
        let short = SysEx7::new(0, Part::Complete, &[0x7E, 0x7F]).unwrap();
        assert_eq!(
            written(&[Message::SysEx7(short).to_packet()]),
            [vec![0xF0, 0x7E, 0x7F, 0xF7]]
        );
    }

    #[test]
    fn packets_are_due_one_block_after_their_block_began() {
        let (mut transmitter, mut queue) = midi_output_queue(8);
        let start = Instant::now();
        // 480 frames at 48 kHz: 10 ms a block, 20 µs a frame.
        transmitter.begin_block_at(start, 480, 48_000.0);
        let note = |key| {
            Packet::from_midi1(
                0,
                MidiMessage::NoteOn {
                    channel: 0,
                    note: key,
                    velocity: 1,
                },
            )
        };
        assert!(transmitter.send(0, note(1)));
        assert!(transmitter.send(240, note(2)));
        let ms = |ms: f64| start + Duration::from_secs_f64(ms / 1_000.0);
        // Within a microsecond: the clock is in whole nanoseconds.
        let near = |at: Option<Instant>, ms: Instant| {
            let at = at.expect("a packet is waiting");
            at.max(ms).duration_since(at.min(ms)) < Duration::from_micros(1)
        };
        let mut sent = Vec::new();
        assert!(near(queue.send_due(ms(9.9), |p| sent.push(*p)), ms(10.0)));
        assert!(sent.is_empty());
        assert!(near(queue.send_due(ms(10.0), |p| sent.push(*p)), ms(15.0)));
        assert_eq!(sent, [note(1)]);
        // A block stamped earlier, as one that comes quickly after a late
        // one is: its packet, due before the one still waiting, keeps its
        // place behind it.
        transmitter.begin_block_at(ms(3.0), 480, 48_000.0);
        assert!(transmitter.send(0, note(3)));
        assert!(near(queue.send_due(ms(14.0), |p| sent.push(*p)), ms(15.0)));
        assert_eq!(sent, [note(1)]);
        assert_eq!(queue.send_due(ms(15.1), |p| sent.push(*p)), None);
        assert_eq!(sent, [note(1), note(2), note(3)]);
    }

    #[test]
    fn a_full_queue_counts_what_it_drops() {
        let (mut transmitter, mut queue) = midi_output_queue(2);
        transmitter.begin_block(64, 48_000.0);
        let packet = Packet::from_midi1(
            0,
            MidiMessage::ProgramChange {
                channel: 0,
                program: 1,
            },
        );
        assert!(transmitter.send(0, packet));
        assert!(transmitter.send(0, packet));
        assert!(!transmitter.send(0, packet));
        assert_eq!((transmitter.dropped(), queue.dropped()), (1, 1));
        let mut count = 0;
        queue.send_all(|_| count += 1);
        assert_eq!(count, 2);
    }

    /// Collects what the thread sends.
    struct Collect(Arc<Mutex<Vec<Vec<u8>>>>);

    impl MidiSender for Collect {
        fn send(&mut self, bytes: &[u8]) -> Result<(), MidiError> {
            self.0.lock().unwrap().push(bytes.to_vec());
            if bytes[0] == 0xC0 {
                return Err(MidiError::Send(String::from("refused")));
            }
            Ok(())
        }
    }

    /// A port that speaks MIDI 2.0: what it was handed, `Some` for each
    /// packet's words and `None` for each flush.
    struct Ump(Sent);

    impl MidiSender for Ump {
        fn send(&mut self, _: &[u8]) -> Result<(), MidiError> {
            panic!("a MIDI 2.0 port is not sent bytes");
        }

        fn protocol(&self) -> MidiProtocol {
            MidiProtocol::Midi2
        }

        fn send_ump(&mut self, words: &[u32]) -> Result<(), MidiError> {
            self.0.lock().unwrap().push(Some(words.to_vec()));
            Ok(())
        }

        fn flush(&mut self) -> Result<(), MidiError> {
            self.0.lock().unwrap().push(None);
            Ok(())
        }
    }

    /// A MIDI 2.0 note with what MIDI 1.0 cannot carry: a 16-bit velocity,
    /// a pitch attribute and a per-note controller.
    fn midi2_note() -> [Packet; 3] {
        let on = ChannelVoice2::NoteOn {
            channel: 2,
            note: 60,
            velocity: 0x1234,
            attribute: NoteAttribute::pitch(60.25),
        };
        let brightness = ChannelVoice2::RegisteredPerNoteController {
            channel: 2,
            note: 60,
            controller: 74,
            value: 0x8765_4321,
        };
        let off = ChannelVoice2::NoteOff {
            channel: 2,
            note: 60,
            velocity: 0,
            attribute: NoteAttribute::NONE,
        };
        [on, brightness, off].map(|message| Message::Midi2 { group: 5, message }.to_packet())
    }

    #[test]
    fn a_midi_2_0_port_gets_the_packets_unchanged_and_together() {
        let sent = Sent::default();
        let (mut transmitter, queue) = midi_output_queue(16);
        let thread = MidiOutputThread::start(Box::new(Ump(Arc::clone(&sent))), queue);
        let [on, brightness, off] = midi2_note();
        let sysex: Vec<Packet> = sysex7_packets(5, &[0x7E; 20]).unwrap().collect();
        transmitter.begin_block(48, 48_000.0);
        for packet in [on, brightness].iter().chain(&sysex) {
            assert!(transmitter.send(0, *packet));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !sent.lock().unwrap().contains(&None) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        // Due on one frame, so handed over at one wake-up, then flushed
        // once: one CoreMIDI event list.
        let mut expected: Vec<Option<Vec<u32>>> = [on, brightness]
            .iter()
            .chain(&sysex)
            .map(|packet| Some(packet.words().to_vec()))
            .collect();
        expected.push(None);
        assert_eq!(*sent.lock().unwrap(), expected);

        // What is still waiting goes when the thread ends, and is flushed.
        transmitter.begin_block(48_000 * 3_600, 48_000.0);
        assert!(transmitter.send(0, off));
        drop(thread);
        expected.extend([Some(off.words().to_vec()), None]);
        assert_eq!(*sent.lock().unwrap(), expected);
    }

    #[test]
    fn a_port_opened_for_midi_2_0_sends_packets_and_bytes_as_packets() {
        let sent = Sent::default();
        let mut output = MidiOutput {
            name: String::from("fake"),
            connection: Connection::Ump {
                port: Box::new(FakePort(Arc::clone(&sent))),
                parser: manguebyte_midi::StreamParser::new(SYSEX_OUT_CAPACITY),
            },
            fallback: None,
        };
        assert_eq!(output.protocol(), MidiProtocol::Midi2);
        assert_eq!(output.fallback(), None);
        let mut writer = PacketWriter::new();
        let [on, ..] = midi2_note();
        output.send_packet(&mut writer, &on).unwrap();
        output
            .send(MidiMessage::NoteOn {
                channel: 1,
                note: 64,
                velocity: 90,
            })
            .unwrap();
        output
            .send_system(manguebyte_midi::SystemMessage::TimingClock)
            .unwrap();
        output
            .send_raw(&[0xF0, 0x7E, 0x7F, 0x09, 0x03, 0xF7])
            .unwrap();
        // A message cut short is not carried into the next call.
        output.send_raw(&[0x90, 60]).unwrap();
        output.send_raw(&[100]).unwrap();
        let words = |packet: Packet| Some(packet.words().to_vec());
        assert_eq!(
            *sent.lock().unwrap(),
            [
                words(on),
                None,
                Some(vec![0x2091_405A]),
                None,
                Some(vec![0x10F8_0000]),
                None,
                // "GM 2 system on" (M2-104-UM, figure 114).
                Some(vec![0x3004_7E7F, 0x0903_0000]),
                None,
                None,
                None,
            ]
        );
    }

    #[test]
    fn a_midi_1_0_port_still_gets_the_default_translation() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (mut transmitter, queue) = midi_output_queue(16);
        let thread = MidiOutputThread::start(Box::new(Collect(Arc::clone(&sent))), queue);
        transmitter.begin_block(48_000 * 3_600, 48_000.0);
        for packet in midi2_note() {
            assert!(transmitter.send(0, packet));
        }
        drop(thread);
        // The pitch attribute, the 16-bit velocity and the per-note
        // controller have no MIDI 1.0 form (M2-104-UM, D.2).
        assert_eq!(
            *sent.lock().unwrap(),
            [vec![0x92, 60, 9], vec![0x82, 60, 0]]
        );
    }

    #[test]
    fn the_thread_sends_packets_on_time_and_what_is_left_when_dropped() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (mut transmitter, queue) = midi_output_queue(16);
        let thread = MidiOutputThread::start(Box::new(Collect(Arc::clone(&sent))), queue);
        assert!(thread.is_running());
        transmitter.begin_block(48, 48_000.0);
        let on = Packet::from_midi1(
            0,
            MidiMessage::NoteOn {
                channel: 0,
                note: 60,
                velocity: 100,
            },
        );
        assert!(transmitter.send(0, on));
        // Wait for the thread to send it: a millisecond after the block.
        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(*sent.lock().unwrap(), [vec![0x90, 60, 100]]);

        // Due an hour from now, and sent all the same when the thread ends.
        transmitter.begin_block(48_000 * 3_600, 48_000.0);
        let program = Packet::from_midi1(
            0,
            MidiMessage::ProgramChange {
                channel: 0,
                program: 3,
            },
        );
        assert!(transmitter.send(0, program));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(sent.lock().unwrap().len(), 1);
        let errors = thread.errors.clone();
        drop(thread);
        assert_eq!(sent.lock().unwrap().last(), Some(&vec![0xC0, 3]));
        // The port refused it, and that was counted.
        assert_eq!(errors.load(Ordering::Relaxed), 1);
    }
}
