//! The device manager's MIDI input ports, merged into the one queue the
//! audio thread reads.
//!
//! The threads the system delivers MIDI on bring each port's messages
//! here, under a lock that the program's thread also takes to open and
//! close ports, and that the audio thread never takes: it only reads the
//! queue, which is wait-free. Only opening and closing a port allocates,
//! and the program's thread does that.
//!
//! This is the one place that knows which port a message came from, so it
//! follows, per port, the notes held and the sustain pedals down, with the
//! tracker the MIDI output port uses. Past it the queue carries one
//! stream; the [module documentation](super#several-input-ports) says what
//! the plugin hears from it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use manguebyte_core::spsc::Producer;
use manguebyte_midi::MidiMessage;

use super::shared::{HeldNotes, NoteChange, SUSTAIN};
use super::{MidiError, MidiInputMessage, MidiSink, Timed};

/// The group every message of the input path is on: it carries MIDI 1.0
/// bytes, which have none.
const GROUP: u8 = 0;

/// The high-resolution velocity prefix (MIDI CA-031).
const VELOCITY_PREFIX: u8 = 88;
const ALL_SOUND_OFF: u8 = 120;
const RESET_ALL_CONTROLLERS: u8 = 121;
/// Controllers 123 to 127: all notes off, and the four mode messages that
/// include it.
const ALL_NOTES_OFF: u8 = 123;

/// What one open port is holding.
struct InputPort {
    key: u64,
    held: HeldNotes,
    /// Notes a channel message of the port ended while their keys were
    /// down, per channel. Another port's note-off for them passes, but
    /// whether they did end only the plugin knows: it may not read that
    /// message at all, and one reading MPE ignores it on a member channel.
    /// So they still get a note-off of their own, once, which is harmless
    /// to a plugin that has ended them.
    ended: [u128; 16],
    /// Channels whose pedal a reset of the controllers let up while it was
    /// down. The same goes for them.
    reset_pedals: u16,
    /// Velocity prefixes waiting for their note, per channel. They wait
    /// here, not in the queue, so that nothing can come between them and
    /// their note there, and they go away with a note-off that is dropped.
    prefixes: [Option<u8>; 16],
}

impl InputPort {
    /// Follows a controller that speaks for a whole channel.
    fn channel_message(&mut self, channel: u8, controller: u8) {
        match controller {
            ALL_SOUND_OFF | ALL_NOTES_OFF..=127 => {
                self.ended[usize::from(channel & 0x0F)] |= self.held.take_channel((GROUP, channel));
            }
            RESET_ALL_CONTROLLERS => {
                if self.held.pedal_down((GROUP, channel)) {
                    self.reset_pedals |= 1 << (channel & 0x0F);
                }
                self.held.follow(NoteChange::Pedal {
                    group: GROUP,
                    channel,
                    down: false,
                });
            }
            _ => {}
        }
    }
}

/// Every open port's notes and the queue to the audio thread. See the
/// [module documentation](self).
pub(crate) struct MergedInput {
    queue: Producer<Timed>,
    ports: Vec<InputPort>,
    next_key: u64,
    /// Note-offs and pedal lifts that found the queue full. Nothing else
    /// is queued until they are, so that none lands after a later note.
    owed: HeldNotes,
    /// The latest time stamp queued. Threads stamp their messages before
    /// they get the lock, and the order here is the one that counts.
    latest: Option<Instant>,
}

impl MergedInput {
    pub(super) fn new(queue: Producer<Timed>) -> Self {
        Self {
            queue,
            ports: Vec::new(),
            next_key: 0,
            owed: HeldNotes::NONE,
            latest: None,
        }
    }

    /// Locks it, whatever a panic on another thread left.
    pub(crate) fn lock(shared: &Mutex<Self>) -> MutexGuard<'_, Self> {
        shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Connects a port through `connect`, which is handed the sink for
    /// its messages.
    pub(crate) fn connect(
        shared: &Arc<Mutex<Self>>,
        connect: impl FnOnce(MidiSink) -> Result<Box<dyn Send>, MidiError>,
    ) -> Result<MergedPort, MidiError> {
        // Messages can come as soon as the port is connected.
        let key = Self::lock(shared).add_port();
        let sink = MidiSink::Merged {
            shared: Arc::clone(shared),
            port: key,
        };
        match connect(sink) {
            Ok(connection) => Ok(MergedPort {
                shared: Arc::clone(shared),
                key,
                connection: Some(connection),
            }),
            Err(error) => {
                Self::lock(shared).close_port(key);
                Err(error)
            }
        }
    }

    pub(super) fn add_port(&mut self) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        self.ports.push(InputPort {
            key,
            held: HeldNotes::NONE,
            ended: [0; 16],
            reset_pedals: 0,
            prefixes: [None; 16],
        });
        key
    }

    /// Whether any open port holds this note.
    fn held(&self, note: (u8, u8, u8)) -> bool {
        self.ports.iter().any(|open| open.held.holds(note))
    }

    /// Whether any open port holds this channel's pedal down.
    fn pedal_held(&self, channel: (u8, u8)) -> bool {
        self.ports.iter().any(|open| open.held.pedal_down(channel))
    }

    /// Queues one message of `port`, as the
    /// [module documentation](super#several-input-ports) says. Neither
    /// allocates nor waits for anything but the lock already held.
    pub(super) fn receive(&mut self, port: u64, mut timed: Timed) {
        let Some(index) = self.ports.iter().position(|open| open.key == port) else {
            return;
        };
        timed.at = self.stamp(timed.at);
        // Whatever becomes of this message, what is owed goes first.
        self.settle(timed.at);
        let MidiInputMessage::Channel(message) = timed.message else {
            self.send(timed);
            return;
        };
        let change = NoteChange::of_midi1(GROUP, &message);
        match change {
            NoteChange::On {
                group,
                channel,
                key,
            } => {
                let at = timed.at;
                // Struck again: the note it replaces ends first, so that
                // every note-on the plugin gets has one note-off. All of it
                // goes or none does, in which case the note goes on as if
                // this had not been played, and its note-off is still to
                // come.
                let strike = self.held((group, channel, key));
                let prefix = self.ports[index].prefixes[usize::from(channel & 0x0F)].take();
                let room = 1 + usize::from(strike) + usize::from(prefix.is_some());
                if !self.has_room(room) {
                    return;
                }
                if strike {
                    self.push(note_off(at, channel, key));
                }
                self.push_prefix(at, channel, prefix);
                self.push(timed);
                let open = &mut self.ports[index];
                open.held.follow(change);
                open.ended[usize::from(channel & 0x0F)] &= !(1 << (key & 0x7F));
            }
            NoteChange::Off {
                group,
                channel,
                key,
            } => {
                let open = &mut self.ports[index];
                let prefix = open.prefixes[usize::from(channel & 0x0F)].take();
                let held = open.held.follow(change);
                let ended = &mut open.ended[usize::from(channel & 0x0F)];
                let bit = 1 << (key & 0x7F);
                let told_to_end = *ended & bit != 0;
                *ended &= !bit;
                // A note the port does not hold has had its note-off, such
                // as one its closing ended. A note another port holds goes
                // on until that port lets go.
                if !(held || told_to_end) || self.held((group, channel, key)) {
                    return;
                }
                if self.has_room(1 + usize::from(prefix.is_some())) {
                    self.push_prefix(timed.at, channel, prefix);
                    self.push(timed);
                } else {
                    self.owed.follow(NoteChange::On {
                        group,
                        channel,
                        key,
                    });
                }
            }
            NoteChange::Pedal {
                channel,
                down: true,
                ..
            } => {
                if self.send(timed) {
                    let open = &mut self.ports[index];
                    open.held.follow(change);
                    open.reset_pedals &= !(1 << (channel & 0x0F));
                }
            }
            NoteChange::Pedal {
                group,
                channel,
                down: false,
            } => {
                let open = &mut self.ports[index];
                open.held.follow(change);
                open.reset_pedals &= !(1 << (channel & 0x0F));
                if !self.pedal_held((group, channel)) && !self.send(timed) {
                    self.owed.follow(NoteChange::Pedal {
                        group,
                        channel,
                        down: true,
                    });
                }
            }
            NoteChange::Other => {
                let MidiMessage::ControlChange {
                    channel,
                    controller,
                    value,
                } = message
                else {
                    self.send(timed);
                    return;
                };
                if controller == VELOCITY_PREFIX {
                    self.ports[index].prefixes[usize::from(channel & 0x0F)] = Some(value);
                } else if self.send(timed) {
                    // Only what reached the plugin: if this did not, the
                    // notes and the pedals it speaks of are all still there.
                    self.ports[index].channel_message(channel, controller);
                }
            }
        }
    }

    /// Takes a port out, once its connection is gone: every note it holds
    /// that no other port does gets a note-off, then every pedal it holds
    /// down that no other port does is lifted, and the same for those a
    /// channel message of the port ended, which the plugin may not have
    /// taken up.
    pub(super) fn close_port(&mut self, port: u64) {
        let Some(index) = self.ports.iter().position(|open| open.key == port) else {
            return;
        };
        let InputPort {
            mut held,
            ended,
            reset_pedals,
            ..
        } = self.ports.remove(index);
        let (notes, pedals) = held.drain();
        for (group, channel, key) in notes {
            if !self.held((group, channel, key)) {
                self.owed.follow(NoteChange::On {
                    group,
                    channel,
                    key,
                });
            }
        }
        for (channel, keys) in (0..16u8).zip(ended) {
            for key in (0..128u8).filter(|&key| keys & (1u128 << key) != 0) {
                if !self.held((GROUP, channel, key)) {
                    self.owed.follow(NoteChange::On {
                        group: GROUP,
                        channel,
                        key,
                    });
                }
            }
        }
        let reset = (0..16u8)
            .filter(|&channel| reset_pedals & (1 << channel) != 0)
            .map(|channel| (GROUP, channel));
        for (group, channel) in pedals.into_iter().chain(reset) {
            if !self.pedal_held((group, channel)) {
                self.owed.follow(NoteChange::Pedal {
                    group,
                    channel,
                    down: true,
                });
            }
        }
        self.retry_owed();
    }

    /// Queues what is owed, as far as there is room. For the program's
    /// thread, so that what found the queue full does not wait for a
    /// port's next message.
    pub(crate) fn retry_owed(&mut self) {
        let now = self.stamp(Instant::now());
        self.settle(now);
    }

    /// `at`, or the latest time stamp queued if that is later.
    fn stamp(&mut self, at: Instant) -> Instant {
        let at = self.latest.map_or(at, |latest| latest.max(at));
        self.latest = Some(at);
        at
    }

    /// Queues what is owed, and then `timed` if all of it went. Says
    /// whether `timed` was queued.
    fn send(&mut self, timed: Timed) -> bool {
        self.settle(timed.at) && self.queue.push(timed).is_ok()
    }

    /// Whether nothing is owed and `count` messages fit in the queue. Only
    /// the thread that holds this lock adds to the queue, and the audio
    /// thread only makes more room, so that many [`push`](Self::push)es
    /// then cannot fail.
    fn has_room(&self, count: usize) -> bool {
        self.owed.first_note().is_none()
            && self.owed.first_pedal().is_none()
            && self.queue.free_len() >= count
    }

    /// Queues a message [`has_room`](Self::has_room) has made room for.
    fn push(&mut self, timed: Timed) {
        let _ = self.queue.push(timed);
    }

    /// Queues a velocity prefix, if there is one, that
    /// [`has_room`](Self::has_room) has made room for.
    fn push_prefix(&mut self, at: Instant, channel: u8, prefix: Option<u8>) {
        if let Some(value) = prefix {
            self.push(channel_message(
                at,
                MidiMessage::ControlChange {
                    channel,
                    controller: VELOCITY_PREFIX,
                    value,
                },
            ));
        }
    }

    /// Queues what is owed, the note-offs first, as far as there is room.
    /// Says whether all of it went.
    fn settle(&mut self, at: Instant) -> bool {
        while let Some((group, channel, key)) = self.owed.first_note() {
            if self.queue.push(note_off(at, channel, key)).is_err() {
                return false;
            }
            self.owed.follow(NoteChange::Off {
                group,
                channel,
                key,
            });
        }
        while let Some((group, channel)) = self.owed.first_pedal() {
            let lift = MidiMessage::ControlChange {
                channel,
                controller: SUSTAIN,
                value: 0,
            };
            if self.queue.push(channel_message(at, lift)).is_err() {
                return false;
            }
            self.owed.follow(NoteChange::Pedal {
                group,
                channel,
                down: false,
            });
        }
        true
    }
}

fn channel_message(at: Instant, message: MidiMessage) -> Timed {
    Timed {
        at,
        message: MidiInputMessage::Channel(message),
    }
}

fn note_off(at: Instant, channel: u8, note: u8) -> Timed {
    channel_message(
        at,
        MidiMessage::NoteOff {
            channel,
            note,
            velocity: 0,
        },
    )
}

/// A port connected to a [`MergedInput`]. Dropping it closes the port,
/// whatever the reason, and then ends what the port held.
pub(crate) struct MergedPort {
    shared: Arc<Mutex<MergedInput>>,
    key: u64,
    connection: Option<Box<dyn Send>>,
}

impl Drop for MergedPort {
    fn drop(&mut self) {
        // The connection goes first, and without the lock: closing it can
        // wait for a callback that is waiting for the lock, and once it is
        // gone nothing more arrives from the port.
        self.connection = None;
        MergedInput::lock(&self.shared).close_port(self.key);
    }
}
