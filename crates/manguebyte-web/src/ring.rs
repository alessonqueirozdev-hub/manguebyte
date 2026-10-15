//! A single-producer single-consumer ring of [`EventRecord`]s.
//!
//! The ring has a fixed layout, so the same ring can live in a
//! `SharedArrayBuffer` that a browser's main thread and its audio worklet
//! both see. `js/ring.js` implements that side over the same bytes; this
//! module implements it in Rust, for native programs and as the reference
//! the JavaScript is tested against.
//!
//! # Layout
//!
//! A ring of capacity `C` (a power of two) takes
//! [`ring_bytes`]`(C) = 16 + 32·C` bytes:
//!
//! | Bytes      | Contents |
//! |------------|----------|
//! | 0..4       | `head`: records ever taken out, `u32`, written only by the consumer |
//! | 4..8       | `tail`: records ever put in, `u32`, written only by the producer |
//! | 8..12      | capacity `C`, `u32`, written once when the ring is made |
//! | 12..16     | [`RING_MAGIC`], `u32`, written once when the ring is made |
//! | 16..16+32C | `C` slots of 32 bytes, each an [`EventRecord`] |
//!
//! The counters run freely and wrap around at 2³²; `tail - head` (wrapping)
//! is the number of records waiting, and record number `n` lives in slot
//! `n mod C`. The ring is empty when the counters are equal and full when
//! they are `C` apart.
//!
//! The producer writes a slot, then publishes it by storing the new `tail`
//! with release ordering (in JavaScript, `Atomics.store`). The consumer
//! loads `tail` with acquire ordering, reads the slots, then hands them
//! back by storing the new `head` with release ordering. Neither side ever
//! waits for the other.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::record::{EventRecord, RECORD_SIZE};

/// Bytes before the first slot.
pub const RING_HEADER_BYTES: usize = 16;

/// The value in bytes 12..16 of a ring: the ASCII text `MBR1`, read as a
/// little-endian `u32`.
pub const RING_MAGIC: u32 = u32::from_le_bytes(*b"MBR1");

/// The largest capacity a ring can have.
pub const RING_MAX_CAPACITY: usize = 1 << 24;

/// Index of each `u32` word in a ring's header.
pub mod header {
    /// Records ever taken out.
    pub const HEAD: usize = 0;
    /// Records ever put in.
    pub const TAIL: usize = 1;
    /// The capacity.
    pub const CAPACITY: usize = 2;
    /// [`RING_MAGIC`](super::RING_MAGIC).
    pub const MAGIC: usize = 3;
}

/// How many bytes a ring of `capacity` records takes.
pub const fn ring_bytes(capacity: usize) -> usize {
    RING_HEADER_BYTES + RECORD_SIZE * capacity
}

struct Shared {
    header: [AtomicU32; 4],
    /// Each slot as four little-endian 64-bit words. Slots are atomics so
    /// that both sides can touch them without `unsafe`; the ordering that
    /// matters is carried by `head` and `tail`.
    slots: Box<[[AtomicU64; 4]]>,
    mask: u32,
}

/// The writing half of a ring. See the [module](self) documentation.
pub struct RingProducer {
    shared: Arc<Shared>,
}

/// The reading half of a ring. See the [module](self) documentation.
pub struct RingConsumer {
    shared: Arc<Shared>,
}

/// Creates a ring that holds `capacity` records, rounded up to a power of
/// two. Allocates; the halves never do.
///
/// ```
/// use manguebyte_web::{EventRecord, event_ring};
///
/// let (mut producer, mut consumer) = event_ring(4);
/// producer.push(EventRecord::note_on(10, 0, 60, 1.0)).unwrap();
/// assert_eq!(consumer.pop().map(|record| record.frame), Some(10));
/// assert_eq!(consumer.pop(), None);
/// ```
///
/// # Panics
///
/// Panics if `capacity` is zero or above [`RING_MAX_CAPACITY`].
pub fn event_ring(capacity: usize) -> (RingProducer, RingConsumer) {
    event_ring_starting_at(capacity, 0)
}

/// Like [`event_ring`], with both counters starting at `start`, as if
/// `start` records had already passed through. A ring whose counters are
/// about to wrap around 2³² behaves exactly like a new one; this is how
/// that is tested.
///
/// # Panics
///
/// Panics if `capacity` is zero or above [`RING_MAX_CAPACITY`].
pub fn event_ring_starting_at(capacity: usize, start: u32) -> (RingProducer, RingConsumer) {
    assert!(
        capacity > 0 && capacity <= RING_MAX_CAPACITY,
        "a ring holds 1 to {RING_MAX_CAPACITY} records, not {capacity}"
    );
    let capacity = capacity.next_power_of_two();
    let slots = (0..capacity)
        .map(|_| core::array::from_fn(|_| AtomicU64::new(0)))
        .collect();
    let shared = Arc::new(Shared {
        header: [
            AtomicU32::new(start),
            AtomicU32::new(start),
            AtomicU32::new(capacity as u32),
            AtomicU32::new(RING_MAGIC),
        ],
        slots,
        mask: capacity as u32 - 1,
    });
    (
        RingProducer {
            shared: Arc::clone(&shared),
        },
        RingConsumer { shared },
    )
}

impl Shared {
    fn capacity(&self) -> u32 {
        self.mask + 1
    }

    fn waiting(&self) -> u32 {
        let head = self.header[header::HEAD].load(Ordering::Acquire);
        let tail = self.header[header::TAIL].load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    /// The header and every slot as bytes, laid out as in a
    /// `SharedArrayBuffer`.
    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(ring_bytes(self.slots.len()));
        for word in &self.header {
            bytes.extend_from_slice(&word.load(Ordering::Acquire).to_le_bytes());
        }
        for slot in self.slots.iter() {
            for word in slot {
                bytes.extend_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
            }
        }
        bytes
    }
}

impl RingProducer {
    /// Puts a record in. Real-time safe and wait-free.
    ///
    /// # Errors
    ///
    /// Gives the record back if the ring is full.
    pub fn push(&mut self, record: EventRecord) -> Result<(), EventRecord> {
        let shared = &*self.shared;
        let head = shared.header[header::HEAD].load(Ordering::Acquire);
        let tail = shared.header[header::TAIL].load(Ordering::Relaxed);
        if tail.wrapping_sub(head) >= shared.capacity() {
            return Err(record);
        }
        let slot = &shared.slots[(tail & shared.mask) as usize];
        for (cell, word) in slot.iter().zip(record.to_words()) {
            cell.store(word, Ordering::Relaxed);
        }
        shared.header[header::TAIL].store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// How many more records fit right now.
    pub fn space(&self) -> usize {
        (self.shared.capacity() - self.shared.waiting()) as usize
    }

    /// How many records the ring holds when full.
    pub fn capacity(&self) -> usize {
        self.shared.capacity() as usize
    }

    /// The ring's bytes as a `SharedArrayBuffer` holding it would contain
    /// them. Allocates; for tests and for checking the layout.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.shared.to_bytes()
    }
}

impl RingConsumer {
    /// Takes the oldest record out. Real-time safe and wait-free.
    pub fn pop(&mut self) -> Option<EventRecord> {
        let shared = &*self.shared;
        let head = shared.header[header::HEAD].load(Ordering::Relaxed);
        let tail = shared.header[header::TAIL].load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let slot = &shared.slots[(head & shared.mask) as usize];
        let mut words = [0u64; 4];
        for (word, cell) in words.iter_mut().zip(slot) {
            *word = cell.load(Ordering::Relaxed);
        }
        shared.header[header::HEAD].store(head.wrapping_add(1), Ordering::Release);
        Some(EventRecord::from_words(words))
    }

    /// How many records are waiting.
    pub fn len(&self) -> usize {
        self.shared.waiting() as usize
    }

    /// Whether no record is waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many records the ring holds when full.
    pub fn capacity(&self) -> usize {
        self.shared.capacity() as usize
    }
}

impl core::fmt::Debug for RingProducer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RingProducer")
            .field("capacity", &self.capacity())
            .field("space", &self.space())
            .finish()
    }
}

impl core::fmt::Debug for RingConsumer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RingConsumer")
            .field("capacity", &self.capacity())
            .field("len", &self.len())
            .finish()
    }
}
