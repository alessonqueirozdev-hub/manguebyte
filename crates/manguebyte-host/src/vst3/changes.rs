//! The objects a plugin reads and writes while it processes: parameter
//! changes and event lists.
//!
//! VST 3 hands a block's automation to a plugin as an object holding one
//! queue of points per parameter, and its notes as an event list; the
//! plugin answers through two more objects of the same kinds. All four are
//! called on the audio thread, in the middle of `process`, so they must
//! not allocate, and they are made once, when the plugin is activated.
//!
//! Everything in them is stored in atomics. Nothing here is `unsafe` beyond
//! writing through the pointers a plugin passes, and a plugin that calls
//! from a thread it should not can garble a value and nothing more.

// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast that is needed on one platform is redundant on the
// other.
#![allow(clippy::unnecessary_cast)]

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use vst3::Steinberg::Vst::Event_::EventTypes_;
use vst3::Steinberg::Vst::{
    self as vst, IEventList, IEventListTrait, IParamValueQueue, IParamValueQueueTrait,
    IParameterChanges, IParameterChangesTrait,
};
use vst3::Steinberg::{kInvalidArgument, kResultFalse, kResultOk, tresult};
use vst3::{Class, ComWrapper};

use crate::threads::guard;

// --- Parameter changes ----------------------------------------------------

/// One automation point as the audio thread gathers it, before it is put
/// where the plugin reads it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Point {
    /// The parameter.
    pub(crate) id: u32,
    /// Where the point came in the block's events, so that points of one
    /// parameter keep their order.
    pub(crate) order: u32,
    /// Frame within the piece.
    pub(crate) offset: i32,
    /// Normalized value.
    pub(crate) value: f64,
}

#[derive(Default)]
struct PointCell {
    offset: AtomicI32,
    value: AtomicU64,
}

/// The points of every queue of one list, side by side.
struct Arena {
    cells: Box<[PointCell]>,
}

/// The automation points of one parameter in one block.
pub(crate) struct ParamQueue {
    arena: Arc<Arena>,
    id: AtomicU32,
    /// Where this queue's points start in the arena.
    start: AtomicU32,
    /// How many it has.
    len: AtomicU32,
    /// How many it has room for.
    room: AtomicU32,
    /// Points refused for want of room.
    refused: Arc<AtomicU32>,
}

impl ParamQueue {
    fn cell(&self, index: u32) -> Option<&PointCell> {
        let at = self.start.load(Ordering::Relaxed).checked_add(index)?;
        self.arena.cells.get(at as usize)
    }
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> vst::ParamID {
        self.id.load(Ordering::Relaxed)
    }

    unsafe fn getPointCount(&self) -> i32 {
        self.len.load(Ordering::Relaxed) as i32
    }

    unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut f64) -> tresult {
        guard(kResultFalse, || {
            let Ok(index) = u32::try_from(index) else {
                return kInvalidArgument;
            };
            if index >= self.len.load(Ordering::Relaxed) {
                return kResultFalse;
            }
            let Some(cell) = self.cell(index) else {
                return kResultFalse;
            };
            // SAFETY: the plugin's own numbers, or null.
            unsafe {
                if !offset.is_null() {
                    offset.write_unaligned(cell.offset.load(Ordering::Relaxed));
                }
                if !value.is_null() {
                    value.write_unaligned(f64::from_bits(cell.value.load(Ordering::Relaxed)));
                }
            }
            kResultOk
        })
    }

    unsafe fn addPoint(&self, offset: i32, value: f64, index: *mut i32) -> tresult {
        guard(kResultFalse, || {
            let len = self.len.load(Ordering::Relaxed);
            let cell = (len < self.room.load(Ordering::Relaxed))
                .then(|| self.cell(len))
                .flatten();
            let Some(cell) = cell else {
                self.refused.fetch_add(1, Ordering::Relaxed);
                return kResultFalse;
            };
            cell.offset.store(offset, Ordering::Relaxed);
            cell.value.store(value.to_bits(), Ordering::Relaxed);
            self.len.store(len + 1, Ordering::Relaxed);
            if !index.is_null() {
                // SAFETY: the plugin's own number.
                unsafe { index.write_unaligned(len as i32) };
            }
            kResultOk
        })
    }
}

/// The parameter changes of one block: a list of queues, one per parameter
/// that changes.
pub(crate) struct ParamChanges {
    queues: Box<[ComWrapper<ParamQueue>]>,
    arena: Arc<Arena>,
    count: AtomicU32,
    /// How many points a queue the plugin adds has room for, or zero for a
    /// list the plugin may only read.
    points_per_added_queue: u32,
    /// Queues and points refused for want of room.
    refused: Arc<AtomicU32>,
}

impl ParamChanges {
    fn new(queues: usize, points: usize, points_per_added_queue: u32) -> ComWrapper<Self> {
        let arena = Arc::new(Arena {
            cells: (0..points).map(|_| PointCell::default()).collect(),
        });
        let refused = Arc::new(AtomicU32::new(0));
        let queues = (0..queues)
            .map(|_| {
                ComWrapper::new(ParamQueue {
                    arena: Arc::clone(&arena),
                    id: AtomicU32::new(0),
                    start: AtomicU32::new(0),
                    len: AtomicU32::new(0),
                    room: AtomicU32::new(0),
                    refused: Arc::clone(&refused),
                })
            })
            .collect();
        ComWrapper::new(Self {
            queues,
            arena,
            count: AtomicU32::new(0),
            points_per_added_queue,
            refused,
        })
    }

    /// A list for the host to fill and the plugin to read: up to `queues`
    /// parameters and `points` points between them. Allocates.
    pub(crate) fn for_input(queues: usize, points: usize) -> ComWrapper<Self> {
        Self::new(queues, points, 0)
    }

    /// A list for the plugin to fill: up to `queues` parameters, each with
    /// room for `points_per_queue` points. Allocates.
    pub(crate) fn for_output(queues: usize, points_per_queue: usize) -> ComWrapper<Self> {
        Self::new(
            queues,
            queues.saturating_mul(points_per_queue),
            points_per_queue as u32,
        )
    }

    /// Empties the list. Real-time safe.
    pub(crate) fn clear(&self) {
        self.count.store(0, Ordering::Relaxed);
    }

    /// Fills the list from `points`, which must be sorted by parameter and
    /// then by order, and returns how many of them there was no room for.
    /// Real-time safe.
    pub(crate) fn load(&self, points: &[Point]) -> usize {
        let mut placed = 0usize;
        let mut queues = 0usize;
        let mut lost = 0usize;
        let mut current: Option<&ParamQueue> = None;
        for point in points {
            let Some(cell) = self.arena.cells.get(placed) else {
                lost += 1;
                continue;
            };
            let same = current.is_some_and(|queue| queue.id.load(Ordering::Relaxed) == point.id);
            if !same {
                let Some(queue) = self.queues.get(queues) else {
                    // Every queue is taken. The points are sorted, so the
                    // rest are for further parameters still.
                    lost += 1;
                    continue;
                };
                queue.id.store(point.id, Ordering::Relaxed);
                queue.start.store(placed as u32, Ordering::Relaxed);
                queue.len.store(0, Ordering::Relaxed);
                queue.room.store(0, Ordering::Relaxed);
                current = Some(&**queue);
                queues += 1;
            }
            cell.offset.store(point.offset, Ordering::Relaxed);
            cell.value.store(point.value.to_bits(), Ordering::Relaxed);
            placed += 1;
            if let Some(queue) = current {
                // A queue the host filled is full: the plugin adds nothing.
                queue.len.fetch_add(1, Ordering::Relaxed);
                queue.room.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.store(queues as u32, Ordering::Relaxed);
        lost
    }

    /// Hands every point the plugin added to `visit`, queue by queue, as
    /// `(parameter, frame, value)`. Real-time safe.
    pub(crate) fn drain(&self, mut visit: impl FnMut(u32, i32, f64)) {
        let count = (self.count.load(Ordering::Relaxed) as usize).min(self.queues.len());
        for queue in &self.queues[..count] {
            let id = queue.id.load(Ordering::Relaxed);
            let len = queue
                .len
                .load(Ordering::Relaxed)
                .min(queue.room.load(Ordering::Relaxed));
            for index in 0..len {
                if let Some(cell) = queue.cell(index) {
                    visit(
                        id,
                        cell.offset.load(Ordering::Relaxed),
                        f64::from_bits(cell.value.load(Ordering::Relaxed)),
                    );
                }
            }
        }
    }

    /// How many queues and points were refused since the last call.
    /// Real-time safe.
    pub(crate) fn take_refused(&self) -> u32 {
        self.refused.swap(0, Ordering::Relaxed)
    }

    fn queue_ptr(&self, index: usize) -> *mut IParamValueQueue {
        self.queues
            .get(index)
            .and_then(|queue| queue.as_com_ref::<IParamValueQueue>())
            .map_or(ptr::null_mut(), |queue| queue.as_ptr())
    }
}

/// The pointer a plugin is given to a list of parameter changes.
pub(crate) fn changes_ptr(changes: &ComWrapper<ParamChanges>) -> *mut IParameterChanges {
    changes
        .as_com_ref::<IParameterChanges>()
        .map_or(ptr::null_mut(), |changes| changes.as_ptr())
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> i32 {
        self.count.load(Ordering::Relaxed) as i32
    }

    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        guard(ptr::null_mut(), || {
            let count = self.count.load(Ordering::Relaxed) as usize;
            match usize::try_from(index) {
                Ok(index) if index < count => self.queue_ptr(index),
                _ => ptr::null_mut(),
            }
        })
    }

    unsafe fn addParameterData(
        &self,
        id: *const vst::ParamID,
        index: *mut i32,
    ) -> *mut IParamValueQueue {
        guard(ptr::null_mut(), || {
            if id.is_null() || self.points_per_added_queue == 0 {
                return ptr::null_mut();
            }
            // SAFETY: the plugin passes the parameter's id by reference.
            let id = unsafe { id.read_unaligned() };
            let count = (self.count.load(Ordering::Relaxed) as usize).min(self.queues.len());
            // A parameter has one queue however often the plugin asks.
            let existing = self.queues[..count]
                .iter()
                .position(|queue| queue.id.load(Ordering::Relaxed) == id);
            let at = match existing {
                Some(at) => at,
                None => {
                    let Some(queue) = self.queues.get(count) else {
                        self.refused.fetch_add(1, Ordering::Relaxed);
                        return ptr::null_mut();
                    };
                    queue.id.store(id, Ordering::Relaxed);
                    queue.start.store(
                        count as u32 * self.points_per_added_queue,
                        Ordering::Relaxed,
                    );
                    queue.len.store(0, Ordering::Relaxed);
                    queue
                        .room
                        .store(self.points_per_added_queue, Ordering::Relaxed);
                    self.count.store(count as u32 + 1, Ordering::Relaxed);
                    count
                }
            };
            if !index.is_null() {
                // SAFETY: the plugin's own number.
                unsafe { index.write_unaligned(at as i32) };
            }
            self.queue_ptr(at)
        })
    }
}

// --- Events ---------------------------------------------------------------

/// What one event in a list says. Only the kinds this host sends or keeps
/// are here; the others VST 3 defines carry pointers that are valid for one
/// call and are not kept.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NoteEvent {
    On {
        channel: i16,
        pitch: i16,
        tuning: f32,
        velocity: f32,
        length: i32,
        note_id: i32,
    },
    Off {
        channel: i16,
        pitch: i16,
        velocity: f32,
        note_id: i32,
        tuning: f32,
    },
    Pressure {
        channel: i16,
        pitch: i16,
        pressure: f32,
        note_id: i32,
    },
    Expression {
        type_id: u32,
        note_id: i32,
        value: f64,
    },
    /// A MIDI controller move from a plugin that produces MIDI.
    MidiOut {
        control: u8,
        channel: i8,
        value: i8,
        value2: i8,
    },
}

/// An event with the frame it falls on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TimedEvent {
    pub(crate) offset: i32,
    pub(crate) bus: i32,
    pub(crate) event: NoteEvent,
}

const TAG_ON: u64 = 0;
const TAG_OFF: u64 = 1;
const TAG_PRESSURE: u64 = 2;
const TAG_EXPRESSION: u64 = 3;
const TAG_MIDI_OUT: u64 = 4;

/// Two 32-bit patterns in one word, the first in the low half.
fn pair(low: u32, high: u32) -> u64 {
    u64::from(low) | (u64::from(high) << 32)
}

/// Two 16-bit numbers in one 32-bit pattern, the first in the low half.
fn halves(low: i16, high: i16) -> u32 {
    u32::from(low as u16) | (u32::from(high as u16) << 16)
}

impl TimedEvent {
    /// The event as four words, which is how a list stores it.
    fn to_words(self) -> [u64; 4] {
        let head = |tag: u64| tag | (u64::from(self.offset as u32) << 32);
        let bus = self.bus as u32;
        match self.event {
            NoteEvent::On {
                channel,
                pitch,
                tuning,
                velocity,
                length,
                note_id,
            } => [
                head(TAG_ON),
                pair(bus, note_id as u32),
                pair(halves(channel, pitch), velocity.to_bits()),
                pair(tuning.to_bits(), length as u32),
            ],
            NoteEvent::Off {
                channel,
                pitch,
                velocity,
                note_id,
                tuning,
            } => [
                head(TAG_OFF),
                pair(bus, note_id as u32),
                pair(halves(channel, pitch), velocity.to_bits()),
                pair(tuning.to_bits(), 0),
            ],
            NoteEvent::Pressure {
                channel,
                pitch,
                pressure,
                note_id,
            } => [
                head(TAG_PRESSURE),
                pair(bus, note_id as u32),
                pair(halves(channel, pitch), pressure.to_bits()),
                0,
            ],
            NoteEvent::Expression {
                type_id,
                note_id,
                value,
            } => [
                head(TAG_EXPRESSION),
                pair(bus, note_id as u32),
                pair(type_id, 0),
                value.to_bits(),
            ],
            NoteEvent::MidiOut {
                control,
                channel,
                value,
                value2,
            } => [
                head(TAG_MIDI_OUT),
                pair(bus, 0),
                pair(
                    u32::from_le_bytes([control, channel as u8, value as u8, value2 as u8]),
                    0,
                ),
                0,
            ],
        }
    }

    /// The reverse of [`to_words`](Self::to_words). Words that were not
    /// made by it give `None`.
    fn from_words(words: [u64; 4]) -> Option<Self> {
        let low = |word: u64| word as u32;
        let high = |word: u64| (word >> 32) as u32;
        let first = |pattern: u32| pattern as u16 as i16;
        let second = |pattern: u32| (pattern >> 16) as u16 as i16;
        let note_id = high(words[1]) as i32;
        let (channel, pitch) = (first(low(words[2])), second(low(words[2])));
        let event = match words[0] & 0xFFFF_FFFF {
            TAG_ON => NoteEvent::On {
                channel,
                pitch,
                tuning: f32::from_bits(low(words[3])),
                velocity: f32::from_bits(high(words[2])),
                length: high(words[3]) as i32,
                note_id,
            },
            TAG_OFF => NoteEvent::Off {
                channel,
                pitch,
                velocity: f32::from_bits(high(words[2])),
                note_id,
                tuning: f32::from_bits(low(words[3])),
            },
            TAG_PRESSURE => NoteEvent::Pressure {
                channel,
                pitch,
                pressure: f32::from_bits(high(words[2])),
                note_id,
            },
            TAG_EXPRESSION => NoteEvent::Expression {
                type_id: low(words[2]),
                note_id,
                value: f64::from_bits(words[3]),
            },
            TAG_MIDI_OUT => {
                let [control, channel, value, value2] = low(words[2]).to_le_bytes();
                NoteEvent::MidiOut {
                    control,
                    channel: channel as i8,
                    value: value as i8,
                    value2: value2 as i8,
                }
            }
            _ => return None,
        };
        Some(Self {
            offset: high(words[0]) as i32,
            bus: low(words[1]) as i32,
            event,
        })
    }

    /// The event as VST 3 lays it out.
    fn to_vst(self) -> vst::Event {
        // SAFETY: every field of an event is a number or a pointer, for
        // which all zeros is a value.
        let mut event: vst::Event = unsafe { std::mem::zeroed() };
        event.busIndex = self.bus;
        event.sampleOffset = self.offset;
        match self.event {
            NoteEvent::On {
                channel,
                pitch,
                tuning,
                velocity,
                length,
                note_id,
            } => {
                event.r#type = EventTypes_::kNoteOnEvent as u16;
                event.__field0.noteOn = vst::NoteOnEvent {
                    channel,
                    pitch,
                    tuning,
                    velocity,
                    length,
                    noteId: note_id,
                };
            }
            NoteEvent::Off {
                channel,
                pitch,
                velocity,
                note_id,
                tuning,
            } => {
                event.r#type = EventTypes_::kNoteOffEvent as u16;
                event.__field0.noteOff = vst::NoteOffEvent {
                    channel,
                    pitch,
                    velocity,
                    noteId: note_id,
                    tuning,
                };
            }
            NoteEvent::Pressure {
                channel,
                pitch,
                pressure,
                note_id,
            } => {
                event.r#type = EventTypes_::kPolyPressureEvent as u16;
                event.__field0.polyPressure = vst::PolyPressureEvent {
                    channel,
                    pitch,
                    pressure,
                    noteId: note_id,
                };
            }
            NoteEvent::Expression {
                type_id,
                note_id,
                value,
            } => {
                event.r#type = EventTypes_::kNoteExpressionValueEvent as u16;
                event.__field0.noteExpressionValue = vst::NoteExpressionValueEvent {
                    typeId: type_id,
                    noteId: note_id,
                    value,
                };
            }
            NoteEvent::MidiOut {
                control,
                channel,
                value,
                value2,
            } => {
                event.r#type = EventTypes_::kLegacyMIDICCOutEvent as u16;
                // The SDK's `int8` is bound as `c_char`, which is unsigned on
                // some targets (aarch64 Linux), so convert explicitly.
                event.__field0.midiCCOut = vst::LegacyMIDICCOutEvent {
                    controlNumber: control,
                    channel: channel as std::ffi::c_char,
                    value: value as std::ffi::c_char,
                    value2: value2 as std::ffi::c_char,
                };
            }
        }
        event
    }

    /// Reads an event a plugin made. A kind this host does not keep gives
    /// `None`.
    ///
    /// # Safety
    ///
    /// The member of the event's union that its type names must be the one
    /// that was written.
    unsafe fn from_vst(event: &vst::Event) -> Option<Self> {
        let kind = u32::from(event.r#type);
        // SAFETY: in each arm the type says which member the union holds,
        // per this function's contract.
        let read = unsafe {
            if kind == EventTypes_::kNoteOnEvent as u32 {
                let on = event.__field0.noteOn;
                NoteEvent::On {
                    channel: on.channel,
                    pitch: on.pitch,
                    tuning: on.tuning,
                    velocity: on.velocity,
                    length: on.length,
                    note_id: on.noteId,
                }
            } else if kind == EventTypes_::kNoteOffEvent as u32 {
                let off = event.__field0.noteOff;
                NoteEvent::Off {
                    channel: off.channel,
                    pitch: off.pitch,
                    velocity: off.velocity,
                    note_id: off.noteId,
                    tuning: off.tuning,
                }
            } else if kind == EventTypes_::kPolyPressureEvent as u32 {
                let pressure = event.__field0.polyPressure;
                NoteEvent::Pressure {
                    channel: pressure.channel,
                    pitch: pressure.pitch,
                    pressure: pressure.pressure,
                    note_id: pressure.noteId,
                }
            } else if kind == EventTypes_::kNoteExpressionValueEvent as u32 {
                let shaped = event.__field0.noteExpressionValue;
                NoteEvent::Expression {
                    type_id: shaped.typeId,
                    note_id: shaped.noteId,
                    value: shaped.value,
                }
            } else if kind == EventTypes_::kLegacyMIDICCOutEvent as u32 {
                let midi = event.__field0.midiCCOut;
                // `int8` is `c_char` in the bindings: unsigned on some targets.
                NoteEvent::MidiOut {
                    control: midi.controlNumber,
                    channel: midi.channel as i8,
                    value: midi.value as i8,
                    value2: midi.value2 as i8,
                }
            } else {
                return None;
            }
        };
        Some(Self {
            offset: event.sampleOffset,
            bus: event.busIndex,
            event: read,
        })
    }
}

#[derive(Default)]
struct EventCell([AtomicU64; 4]);

/// The events of one block, in one direction.
pub(crate) struct EventList {
    cells: Box<[EventCell]>,
    count: AtomicU32,
    /// Whether the plugin may add to the list.
    writable: bool,
    /// Events refused for want of room.
    refused: AtomicU32,
}

impl EventList {
    /// A list for the host to fill and the plugin to read. Allocates.
    pub(crate) fn for_input(capacity: usize) -> ComWrapper<Self> {
        Self::new(capacity, false)
    }

    /// A list for the plugin to fill. Allocates.
    pub(crate) fn for_output(capacity: usize) -> ComWrapper<Self> {
        Self::new(capacity, true)
    }

    fn new(capacity: usize, writable: bool) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            cells: (0..capacity).map(|_| EventCell::default()).collect(),
            count: AtomicU32::new(0),
            writable,
            refused: AtomicU32::new(0),
        })
    }

    /// Empties the list. Real-time safe.
    pub(crate) fn clear(&self) {
        self.count.store(0, Ordering::Relaxed);
    }

    /// How many events the list holds.
    pub(crate) fn len(&self) -> usize {
        (self.count.load(Ordering::Relaxed) as usize).min(self.cells.len())
    }

    /// Adds an event at the end, or returns false if the list is full.
    /// Real-time safe.
    pub(crate) fn push(&self, event: TimedEvent) -> bool {
        let count = self.count.load(Ordering::Relaxed);
        let Some(cell) = self.cells.get(count as usize) else {
            return false;
        };
        for (slot, word) in cell.0.iter().zip(event.to_words()) {
            slot.store(word, Ordering::Relaxed);
        }
        self.count.store(count + 1, Ordering::Relaxed);
        true
    }

    /// The event at `index`, if there is one. Real-time safe.
    pub(crate) fn get(&self, index: usize) -> Option<TimedEvent> {
        if index >= self.len() {
            return None;
        }
        let cell = self.cells.get(index)?;
        let mut words = [0u64; 4];
        for (word, slot) in words.iter_mut().zip(&cell.0) {
            *word = slot.load(Ordering::Relaxed);
        }
        TimedEvent::from_words(words)
    }

    /// How many events were refused since the last call. Real-time safe.
    pub(crate) fn take_refused(&self) -> u32 {
        self.refused.swap(0, Ordering::Relaxed)
    }
}

/// The pointer a plugin is given to an event list.
pub(crate) fn events_ptr(events: &ComWrapper<EventList>) -> *mut IEventList {
    events
        .as_com_ref::<IEventList>()
        .map_or(ptr::null_mut(), |events| events.as_ptr())
}

impl Class for EventList {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for EventList {
    unsafe fn getEventCount(&self) -> i32 {
        self.len() as i32
    }

    unsafe fn getEvent(&self, index: i32, event: *mut vst::Event) -> tresult {
        guard(kResultFalse, || {
            let found = usize::try_from(index)
                .ok()
                .and_then(|index| self.get(index));
            let Some(found) = found else {
                return kResultFalse;
            };
            if event.is_null() || !event.is_aligned() {
                return kInvalidArgument;
            }
            // SAFETY: the plugin's own event, checked above, to be written.
            unsafe { event.write(found.to_vst()) };
            kResultOk
        })
    }

    unsafe fn addEvent(&self, event: *mut vst::Event) -> tresult {
        guard(kResultFalse, || {
            if !self.writable {
                return kResultFalse;
            }
            if event.is_null() || !event.is_aligned() {
                return kInvalidArgument;
            }
            // SAFETY: the plugin passes an event it filled in, so the
            // member of the union its type names is the one it wrote.
            let Some(kept) = (unsafe { TimedEvent::from_vst(&*event) }) else {
                // A kind this host has no use for. The plugin could do
                // nothing about a refusal, so it is told the event was
                // taken.
                return kResultOk;
            };
            if self.push(kept) {
                kResultOk
            } else {
                self.refused.fetch_add(1, Ordering::Relaxed);
                kResultFalse
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use vst3::ComPtr;

    use super::*;

    fn point(id: u32, order: u32, offset: i32, value: f64) -> Point {
        Point {
            id,
            order,
            offset,
            value,
        }
    }

    /// Everything a plugin reads out of a list, queue by queue.
    fn read_back(changes: &ComWrapper<ParamChanges>) -> Vec<(u32, Vec<(i32, f64)>)> {
        let list = changes.to_com_ptr::<IParameterChanges>().unwrap();
        let mut seen = Vec::new();
        // SAFETY: calls on this host's own objects with indices below the
        // counts they report and out-pointers to locals.
        unsafe {
            for index in 0..list.getParameterCount() {
                let queue =
                    vst3::ComRef::<IParamValueQueue>::from_raw(list.getParameterData(index))
                        .unwrap();
                let mut points = Vec::new();
                for at in 0..queue.getPointCount() {
                    let (mut offset, mut value) = (0i32, 0f64);
                    assert_eq!(queue.getPoint(at, &mut offset, &mut value), kResultOk);
                    points.push((offset, value));
                }
                seen.push((queue.getParameterId(), points));
            }
        }
        seen
    }

    #[test]
    fn every_point_reaches_the_plugin_in_its_parameters_queue() {
        let changes = ParamChanges::for_input(3, 8);
        let lost = changes.load(&[
            point(5, 0, 0, 0.25),
            point(5, 3, 10, 0.5),
            point(5, 4, 10, 0.75),
            point(9, 1, 3, 1.0),
        ]);
        assert_eq!(lost, 0);
        assert_eq!(
            read_back(&changes),
            [
                (5, vec![(0, 0.25), (10, 0.5), (10, 0.75)]),
                (9, vec![(3, 1.0)])
            ]
        );

        // The next block starts from nothing.
        assert_eq!(changes.load(&[point(2, 0, 7, 0.0)]), 0);
        assert_eq!(read_back(&changes), [(2, vec![(7, 0.0)])]);
        changes.clear();
        assert!(read_back(&changes).is_empty());

        let list = changes.to_com_ptr::<IParameterChanges>().unwrap();
        // SAFETY: calls with numbers and with pointers to locals or null.
        unsafe {
            // Out of range, and a list the plugin may only read.
            assert!(list.getParameterData(0).is_null());
            assert!(list.getParameterData(-1).is_null());
            let (id, mut index) = (1u32, -1i32);
            assert!(list.addParameterData(&id, &mut index).is_null());

            changes.load(&[point(1, 0, 0, 0.5)]);
            let queue =
                vst3::ComRef::<IParamValueQueue>::from_raw(list.getParameterData(0)).unwrap();
            assert_eq!(queue.addPoint(1, 0.0, &mut index), kResultFalse);
            assert_eq!(queue.getPointCount(), 1);
            let (mut offset, mut value) = (0i32, 0f64);
            assert_eq!(queue.getPoint(1, &mut offset, &mut value), kResultFalse);
            assert_eq!(
                queue.getPoint(-1, &mut offset, &mut value),
                kInvalidArgument
            );
            assert_eq!(
                queue.getPoint(0, ptr::null_mut(), ptr::null_mut()),
                kResultOk
            );
        }
    }

    #[test]
    fn points_and_parameters_beyond_the_room_are_counted_not_placed() {
        // Room for two parameters and three points.
        let changes = ParamChanges::for_input(2, 3);
        let lost = changes.load(&[
            point(1, 0, 0, 0.1),
            point(2, 1, 0, 0.2),
            point(3, 2, 0, 0.3),
            point(3, 3, 1, 0.4),
        ]);
        assert_eq!(lost, 2);
        assert_eq!(
            read_back(&changes),
            [(1, vec![(0, 0.1)]), (2, vec![(0, 0.2)])]
        );

        let lost = changes.load(&[
            point(1, 0, 0, 0.1),
            point(1, 1, 1, 0.2),
            point(1, 2, 2, 0.3),
            point(1, 3, 3, 0.4),
            point(2, 4, 0, 0.5),
        ]);
        assert_eq!(lost, 2);
        assert_eq!(
            read_back(&changes),
            [(1, vec![(0, 0.1), (1, 0.2), (2, 0.3)])]
        );
    }

    #[test]
    fn what_the_plugin_reports_is_read_back() {
        let changes = ParamChanges::for_output(2, 2);
        let list = changes.to_com_ptr::<IParameterChanges>().unwrap();
        let mut seen = Vec::new();
        // SAFETY: calls with numbers and with pointers to locals or null.
        unsafe {
            let mut index = -1i32;
            let first = list.addParameterData(&7u32, &mut index);
            assert_eq!(index, 0);
            let first = vst3::ComRef::<IParamValueQueue>::from_raw(first).unwrap();
            assert_eq!(first.addPoint(3, 0.5, &mut index), kResultOk);
            assert_eq!(index, 0);

            let second = list.addParameterData(&9u32, &mut index);
            assert_eq!(index, 1);
            let second = vst3::ComRef::<IParamValueQueue>::from_raw(second).unwrap();
            assert_eq!(second.addPoint(0, 1.0, ptr::null_mut()), kResultOk);

            // Interleaved with another queue, and asked for a second time.
            assert_eq!(first.addPoint(9, 0.75, &mut index), kResultOk);
            assert_eq!(index, 1);
            let again = list.addParameterData(&7u32, &mut index);
            assert_eq!((again, index), (first.as_ptr(), 0));

            // A third point, a third parameter and a null id do not fit.
            assert_eq!(first.addPoint(10, 0.0, &mut index), kResultFalse);
            assert!(list.addParameterData(&11u32, &mut index).is_null());
            assert!(list.addParameterData(ptr::null(), &mut index).is_null());
            assert_eq!(list.getParameterCount(), 2);
        }
        changes.drain(|id, offset, value| seen.push((id, offset, value)));
        assert_eq!(seen, [(7, 3, 0.5), (7, 9, 0.75), (9, 0, 1.0)]);
        assert_eq!(changes.take_refused(), 2);
        assert_eq!(changes.take_refused(), 0);

        changes.clear();
        seen.clear();
        changes.drain(|id, offset, value| seen.push((id, offset, value)));
        assert!(seen.is_empty());
    }

    fn every_kind() -> [TimedEvent; 6] {
        let timed = |offset, bus, event| TimedEvent { offset, bus, event };
        [
            timed(
                0,
                0,
                NoteEvent::On {
                    channel: 15,
                    pitch: 127,
                    tuning: -50.5,
                    velocity: 0.8,
                    length: 0,
                    note_id: -1,
                },
            ),
            timed(
                i32::MAX,
                3,
                NoteEvent::On {
                    channel: -1,
                    pitch: i16::MIN,
                    tuning: f32::MAX,
                    velocity: f32::MIN_POSITIVE,
                    length: i32::MIN,
                    note_id: i32::MAX,
                },
            ),
            timed(
                17,
                0,
                NoteEvent::Off {
                    channel: 3,
                    pitch: 60,
                    velocity: 0.25,
                    note_id: 0x7F00_0000,
                    tuning: 12.0,
                },
            ),
            timed(
                -4,
                1,
                NoteEvent::Pressure {
                    channel: 0,
                    pitch: 1,
                    pressure: 1.0,
                    note_id: 42,
                },
            ),
            timed(
                1_000_000,
                0,
                NoteEvent::Expression {
                    type_id: u32::MAX,
                    note_id: i32::MIN,
                    value: std::f64::consts::PI,
                },
            ),
            timed(
                9,
                2,
                NoteEvent::MidiOut {
                    control: 129,
                    channel: -3,
                    value: 127,
                    value2: -128,
                },
            ),
        ]
    }

    #[test]
    fn an_event_survives_storage_bit_for_bit() {
        for event in every_kind() {
            assert_eq!(TimedEvent::from_words(event.to_words()), Some(event));
            // SAFETY: `to_vst` wrote the member its type names.
            let back = unsafe { TimedEvent::from_vst(&event.to_vst()) };
            assert_eq!(back, Some(event));
        }
        // A number that is not a number keeps its bits too.
        let odd = TimedEvent {
            offset: 0,
            bus: 0,
            event: NoteEvent::Expression {
                type_id: 2,
                note_id: 1,
                value: f64::from_bits(0x7FF8_0000_DEAD_BEEF),
            },
        };
        let Some(TimedEvent {
            event: NoteEvent::Expression { value, .. },
            ..
        }) = TimedEvent::from_words(odd.to_words())
        else {
            panic!("an expression did not come back as one");
        };
        assert_eq!(value.to_bits(), 0x7FF8_0000_DEAD_BEEF);
        assert_eq!(TimedEvent::from_words([99, 0, 0, 0]), None);
    }

    #[test]
    fn a_list_hands_out_what_it_was_given_and_takes_what_fits() {
        let input = EventList::for_input(8);
        for event in every_kind() {
            assert!(input.push(event));
        }
        let list = input.to_com_ptr::<IEventList>().unwrap();
        // SAFETY: calls with numbers and with pointers to locals or null.
        unsafe {
            assert_eq!(list.getEventCount(), 6);
            for (index, expected) in every_kind().into_iter().enumerate() {
                let mut event: vst::Event = std::mem::zeroed();
                assert_eq!(list.getEvent(index as i32, &mut event), kResultOk);
                assert_eq!(TimedEvent::from_vst(&event), Some(expected));
            }
            let mut event: vst::Event = std::mem::zeroed();
            assert_eq!(list.getEvent(6, &mut event), kResultFalse);
            assert_eq!(list.getEvent(-1, &mut event), kResultFalse);
            assert_eq!(list.getEvent(0, ptr::null_mut()), kInvalidArgument);
            // A list the plugin may only read.
            assert_eq!(list.addEvent(&mut event), kResultFalse);
        }
        input.clear();
        assert_eq!(input.len(), 0);
        assert_eq!(input.get(0), None);

        let output = EventList::for_output(2);
        let list: ComPtr<IEventList> = output.to_com_ptr().unwrap();
        // SAFETY: as above.
        unsafe {
            let [mut first, _, mut second, ..] = every_kind().map(TimedEvent::to_vst);
            assert_eq!(list.addEvent(&mut first), kResultOk);
            // A kind that is not kept is taken and forgotten.
            let mut data: vst::Event = std::mem::zeroed();
            data.r#type = EventTypes_::kDataEvent as u16;
            assert_eq!(list.addEvent(&mut data), kResultOk);
            assert_eq!(list.addEvent(&mut second), kResultOk);
            assert_eq!(list.addEvent(&mut second), kResultFalse);
            assert_eq!(list.addEvent(ptr::null_mut()), kInvalidArgument);
        }
        assert_eq!(output.len(), 2);
        assert_eq!(output.get(0), Some(every_kind()[0]));
        assert_eq!(output.get(1), Some(every_kind()[2]));
        assert_eq!(output.take_refused(), 1);
    }
}
