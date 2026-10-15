//! Events that have arrived and wait for their frame.

use core::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use crate::record::Event;

/// One waiting event. Ordered by frame, then by arrival, so events on the
/// same frame are applied in the order they were pushed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Waiting {
    pub(crate) frame: u64,
    pub(crate) sequence: u64,
    pub(crate) event: Event,
}

impl PartialEq for Waiting {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Waiting {}

impl PartialOrd for Waiting {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Waiting {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.frame, self.sequence).cmp(&(other.frame, other.sequence))
    }
}

/// A priority queue of fixed capacity. It never grows, so pushing and
/// popping never allocate.
#[derive(Debug)]
pub(crate) struct Pending {
    heap: BinaryHeap<Reverse<Waiting>>,
    capacity: usize,
    next_sequence: u64,
}

impl Pending {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(capacity),
            capacity,
            next_sequence: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.heap.len()
    }

    pub(crate) fn room(&self) -> usize {
        self.capacity - self.heap.len()
    }

    /// Adds an event, or gives it back if the queue is full.
    pub(crate) fn push(&mut self, frame: u64, event: Event) -> Result<(), Event> {
        if self.heap.len() >= self.capacity {
            return Err(event);
        }
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.heap.push(Reverse(Waiting {
            frame,
            sequence,
            event,
        }));
        Ok(())
    }

    /// Takes the earliest event if its frame is before `end`.
    pub(crate) fn pop_before(&mut self, end: u64) -> Option<Waiting> {
        if self.heap.peek()?.0.frame < end {
            self.heap.pop().map(|Reverse(waiting)| waiting)
        } else {
            None
        }
    }

    pub(crate) fn clear(&mut self) {
        self.heap.clear();
    }
}
