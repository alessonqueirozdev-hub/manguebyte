//! A log that the audio thread can write to.
//!
//! Printing, `String`s and the usual logging crates allocate, lock or make
//! system calls, any of which can make the audio thread miss its deadline.
//! A [`LogQueue`] takes messages from any number of threads, the audio
//! thread included, without any of that, and hands them to a thread that
//! may block, which drains it now and then and does with them what it likes:
//! prints them, writes them to a file, passes them to a host.
//!
//! ```
//! use manguebyte_core::log::{LogLevel, LogQueue};
//!
//! static LOG: LogQueue = LogQueue::new();
//!
//! // On the audio thread: no allocation, no lock, no waiting.
//! let voices = 17;
//! LOG.post_fmt(LogLevel::Warning, format_args!("{voices} voices, one stolen"));
//!
//! // Later, on a thread that may block.
//! let drained = LOG.drain();
//! assert_eq!(drained.records[0].text, "17 voices, one stolen");
//! assert_eq!(drained.lost, 0);
//! ```
//!
//! # How it works
//!
//! Messages go into a fixed set of slots. A writer claims an empty slot with
//! one compare-and-swap, fills it and marks it full; the draining thread
//! claims full slots the same way, reads them and marks them empty. The text
//! is stored in atomics, eight bytes to a word, so there is no `unsafe` code
//! here: the worst a bug could do is garble a message. When every slot is
//! full a message is not written but counted, and the count comes back with
//! the next drain, so a flood is never silent.
//!
//! Each message is numbered when its slot is claimed, and messages come out
//! in that order, across drains as well as within one: a message still
//! being written when a drain passes holds back the ones numbered after it
//! until a later drain. Only the draining side keeps that back, behind a
//! lock that writers never take.
//!
//! Formatting happens on the writing thread, into a buffer on its stack.
//! That is real-time safe for numbers, text and anything else whose
//! `Display` does not allocate. A message below the queue's
//! [level](LogQueue::set_max_level) is dropped before it is formatted, so
//! debug messages left in cost one atomic load when they are turned off.

use core::fmt::{self, Write as _};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

/// Longest message kept, in bytes of UTF-8. Longer ones are cut at the last
/// whole character that fits.
pub const MESSAGE_BYTES: usize = 240;

/// How many messages can wait to be drained at once.
pub const SLOTS: usize = 64;

const WORDS: usize = MESSAGE_BYTES / 8;

const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const FULL: u8 = 2;
const READING: u8 = 3;

/// How much a message matters, from most to least.
///
/// Levels are ordered so that a more severe level is the smaller:
/// `LogLevel::Error < LogLevel::Warning`, and a queue keeps a message when
/// its level is at most the queue's maximum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogLevel {
    /// Something failed.
    Error,
    /// Something is wrong, and work goes on.
    Warning,
    /// Something worth knowing happened.
    Info,
    /// Detail for whoever is finding a fault.
    Debug,
    /// Detail for whoever is following every step.
    Trace,
}

impl LogLevel {
    const ALL: [Self; 5] = [
        Self::Error,
        Self::Warning,
        Self::Info,
        Self::Debug,
        Self::Trace,
    ];

    fn from_index(index: u8) -> Self {
        Self::ALL[usize::from(index).min(Self::ALL.len() - 1)]
    }
}

/// One message taken from a [`LogQueue`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    /// How much it matters.
    pub level: LogLevel,
    /// What it says, at most [`MESSAGE_BYTES`] long.
    pub text: String,
}

/// What one [`drain`](LogQueue::drain) found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainedLog {
    /// The messages, in the order they were posted.
    pub records: Vec<LogRecord>,
    /// How many messages were dropped since the last drain because every
    /// slot was full.
    pub lost: u32,
}

struct Slot {
    state: AtomicU8,
    level: AtomicU8,
    length: AtomicU32,
    /// Position of the message among all messages ever posted.
    order: AtomicU64,
    words: [AtomicU64; WORDS],
}

impl Slot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(EMPTY),
            level: AtomicU8::new(0),
            length: AtomicU32::new(0),
            order: AtomicU64::new(0),
            words: [const { AtomicU64::new(0) }; WORDS],
        }
    }
}

/// What the draining side keeps between drains.
struct Held {
    /// The number of the next message to hand out.
    next: u64,
    /// Messages taken from their slots and numbered after one that was
    /// still being written.
    records: Vec<(u64, LogRecord)>,
}

/// Messages from any thread, the audio thread included, to a thread that
/// may block. See the [module documentation](self).
///
/// Posting is wait-free and never allocates; draining allocates. A queue
/// can be a `static`, since [`new`](Self::new) is `const`.
pub struct LogQueue {
    slots: [Slot; SLOTS],
    next: AtomicU64,
    lost: AtomicU32,
    max_level: AtomicU8,
    held: Mutex<Held>,
}

impl Default for LogQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for LogQueue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LogQueue")
            .field("max_level", &self.max_level())
            .finish_non_exhaustive()
    }
}

impl LogQueue {
    /// An empty queue that keeps messages of every level.
    pub const fn new() -> Self {
        Self {
            slots: [const { Slot::new() }; SLOTS],
            next: AtomicU64::new(0),
            lost: AtomicU32::new(0),
            max_level: AtomicU8::new(LogLevel::Trace as u8),
            held: Mutex::new(Held {
                next: 0,
                records: Vec::new(),
            }),
        }
    }

    /// Keeps only messages at `level` or more severe from now on. Others
    /// are dropped without being formatted or counted. Real-time safe.
    pub fn set_max_level(&self, level: LogLevel) {
        self.max_level.store(level as u8, Ordering::Relaxed);
    }

    /// The least severe level kept.
    pub fn max_level(&self) -> LogLevel {
        LogLevel::from_index(self.max_level.load(Ordering::Relaxed))
    }

    /// Whether a message at `level` would be kept. Real-time safe.
    pub fn enabled(&self, level: LogLevel) -> bool {
        level as u8 <= self.max_level.load(Ordering::Relaxed)
    }

    /// Leaves `text` to be drained, cut to [`MESSAGE_BYTES`].
    ///
    /// Wait-free and free of allocation: at most one pass over the slots.
    /// Returns `false` if the message was not kept, because its level is
    /// filtered out or because every slot is full (then it is counted in
    /// the next drain's [`lost`](DrainedLog::lost)).
    pub fn post(&self, level: LogLevel, text: &str) -> bool {
        self.enabled(level) && self.store(level, cut(text, MESSAGE_BYTES).as_bytes())
    }

    /// Formats `message` and leaves it to be drained, cut to
    /// [`MESSAGE_BYTES`].
    ///
    /// The text is formatted on this thread into a buffer on its stack,
    /// which is real-time safe as long as the values' `Display` and `Debug`
    /// do not allocate; numbers, `str` and `bool` do not. Formatting stops
    /// when the buffer is full. A message whose level is filtered out is not
    /// formatted at all. Returns what [`post`](Self::post) returns.
    pub fn post_fmt(&self, level: LogLevel, message: fmt::Arguments<'_>) -> bool {
        if !self.enabled(level) {
            return false;
        }
        let mut buffer = StackText::new();
        // An error only means the text was cut, which is what is wanted.
        let _ = buffer.write_fmt(message);
        self.store(level, buffer.as_bytes())
    }

    fn store(&self, level: LogLevel, text: &[u8]) -> bool {
        for slot in &self.slots {
            if slot
                .state
                .compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            slot.level.store(level as u8, Ordering::Relaxed);
            slot.length.store(text.len() as u32, Ordering::Relaxed);
            slot.order
                .store(self.next.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
            for (word, chunk) in slot.words.iter().zip(text.chunks(8)) {
                let mut bytes = [0u8; 8];
                bytes[..chunk.len()].copy_from_slice(chunk);
                word.store(u64::from_le_bytes(bytes), Ordering::Relaxed);
            }
            // Publishes everything stored above to whoever claims the slot.
            slot.state.store(FULL, Ordering::Release);
            return true;
        }
        self.lost.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// Takes the waiting messages, oldest first, and the number lost since
    /// the last drain. Allocates and may wait for another drain; call it
    /// from a thread that may block.
    ///
    /// Messages come out in the order they were posted, across drains: one
    /// still being written holds back those posted after it, which come out
    /// with it in a later drain. Any thread may drain; each message comes
    /// out once.
    pub fn drain(&self) -> DrainedLog {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let mut found = std::mem::take(&mut held.records);
        for slot in &self.slots {
            if slot
                .state
                .compare_exchange(FULL, READING, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            let length = (slot.length.load(Ordering::Relaxed) as usize).min(MESSAGE_BYTES);
            let mut bytes = [0u8; MESSAGE_BYTES];
            for (chunk, word) in bytes.chunks_mut(8).zip(&slot.words) {
                chunk.copy_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
            }
            found.push((
                slot.order.load(Ordering::Relaxed),
                LogRecord {
                    level: LogLevel::from_index(slot.level.load(Ordering::Relaxed)),
                    text: String::from_utf8_lossy(&bytes[..length]).into_owned(),
                },
            ));
            // Hands the slot back only after everything above was read.
            slot.state.store(EMPTY, Ordering::Release);
        }
        found.sort_by_key(|&(order, _)| order);
        // Every number handed out belongs to a message that will be
        // published, so the run without gaps can go and the rest waits.
        let mut ready = 0;
        while found
            .get(ready)
            .is_some_and(|&(order, _)| order == held.next)
        {
            held.next += 1;
            ready += 1;
        }
        held.records = found.split_off(ready);
        DrainedLog {
            records: found.into_iter().map(|(_, record)| record).collect(),
            lost: self.lost.swap(0, Ordering::Relaxed),
        }
    }
}

/// The longest start of `text` that is at most `limit` bytes and ends on a
/// character boundary.
fn cut(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Text formatted on the stack, cut at the last whole character that fits.
struct StackText {
    bytes: [u8; MESSAGE_BYTES],
    len: usize,
}

impl StackText {
    const fn new() -> Self {
        Self {
            bytes: [0; MESSAGE_BYTES],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl fmt::Write for StackText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = MESSAGE_BYTES - self.len;
        let taken = cut(text, room);
        self.bytes[self.len..self.len + taken.len()].copy_from_slice(taken.as_bytes());
        self.len += taken.len();
        // Stops the formatting once nothing more fits.
        if taken.len() < text.len() {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn messages_come_out_whole_in_order_and_with_their_level() {
        let log = LogQueue::new();
        assert!(log.post(LogLevel::Info, "first"));
        assert!(log.post(LogLevel::Error, ""));
        assert!(log.post_fmt(LogLevel::Debug, format_args!("{}: {:.2}", "gain", 0.5)));
        assert!(log.post(LogLevel::Trace, "with an accent: \u{e7}"));
        let drained = log.drain();
        let got: Vec<(LogLevel, &str)> = drained
            .records
            .iter()
            .map(|record| (record.level, record.text.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (LogLevel::Info, "first"),
                (LogLevel::Error, ""),
                (LogLevel::Debug, "gain: 0.50"),
                (LogLevel::Trace, "with an accent: \u{e7}"),
            ]
        );
        assert_eq!(drained.lost, 0);
        assert_eq!(log.drain(), DrainedLog::default(), "nothing is seen twice");
    }

    #[test]
    fn a_long_message_is_cut_at_a_whole_character() {
        let log = LogQueue::new();
        // A two-byte character straddles the limit: it is left out whole.
        let long = format!("{}\u{e7}tail", "a".repeat(MESSAGE_BYTES - 1));
        log.post(LogLevel::Info, &long);
        log.post_fmt(LogLevel::Info, format_args!("{long}"));
        // Formatted in pieces, the cut falls inside a later piece.
        log.post_fmt(
            LogLevel::Info,
            format_args!("{}{}", "b".repeat(MESSAGE_BYTES - 2), "\u{20ac}"),
        );
        let drained = log.drain();
        let expected = "a".repeat(MESSAGE_BYTES - 1);
        assert_eq!(drained.records[0].text, expected);
        assert_eq!(drained.records[1].text, expected);
        assert_eq!(drained.records[2].text, "b".repeat(MESSAGE_BYTES - 2));
    }

    #[test]
    fn a_full_queue_counts_what_it_drops_and_has_room_again_once_drained() {
        let log = LogQueue::new();
        for index in 0..SLOTS {
            assert!(log.post_fmt(LogLevel::Info, format_args!("{index}")));
        }
        assert!(!log.post(LogLevel::Error, "no room"));
        assert!(!log.post(LogLevel::Error, "no room either"));
        let drained = log.drain();
        assert_eq!(drained.records.len(), SLOTS);
        assert_eq!(drained.records[SLOTS - 1].text, (SLOTS - 1).to_string());
        assert_eq!(drained.lost, 2);
        assert!(log.post(LogLevel::Info, "room again"));
        assert_eq!(log.drain().lost, 0, "the count starts again");
    }

    #[test]
    fn a_filtered_message_is_neither_formatted_nor_counted() {
        struct Counted<'a>(&'a AtomicU32);
        impl fmt::Display for Counted<'_> {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fetch_add(1, Ordering::Relaxed);
                formatter.write_str("counted")
            }
        }
        let formatted = AtomicU32::new(0);
        let log = LogQueue::new();
        log.set_max_level(LogLevel::Warning);
        assert_eq!(log.max_level(), LogLevel::Warning);
        assert!(log.enabled(LogLevel::Error) && !log.enabled(LogLevel::Info));
        assert!(!log.post_fmt(LogLevel::Debug, format_args!("{}", Counted(&formatted))));
        assert!(!log.post(LogLevel::Info, "dropped"));
        assert_eq!(formatted.load(Ordering::Relaxed), 0);
        assert!(log.post_fmt(LogLevel::Warning, format_args!("{}", Counted(&formatted))));
        assert_eq!(formatted.load(Ordering::Relaxed), 1);
        let drained = log.drain();
        assert_eq!(drained.records.len(), 1);
        assert_eq!(drained.lost, 0);
    }

    #[test]
    fn several_threads_can_post_while_another_drains() {
        const THREADS: usize = 4;
        const EACH: usize = if cfg!(miri) { 8 } else { 200 };
        let log = Arc::new(LogQueue::new());
        let writers: Vec<_> = (0..THREADS)
            .map(|thread| {
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    for index in 0..EACH {
                        while !log.post_fmt(LogLevel::Info, format_args!("{thread} {index}")) {
                            std::thread::yield_now();
                        }
                    }
                })
            })
            .collect();
        let mut seen = vec![Vec::new(); THREADS];
        // Writers retry whatever did not fit, so what was counted as lost on
        // the way is not checked.
        let mut collect = |drained: DrainedLog| {
            for record in drained.records {
                let mut words = record.text.split(' ');
                let thread: usize = words.next().unwrap().parse().unwrap();
                let index: usize = words.next().unwrap().parse().unwrap();
                seen[thread].push(index);
            }
        };
        while writers.iter().any(|writer| !writer.is_finished()) {
            collect(log.drain());
            std::thread::yield_now();
        }
        for writer in writers {
            writer.join().unwrap();
        }
        collect(log.drain());
        // Every message arrived once, and each thread's in its order, across
        // drains.
        for indices in seen {
            assert_eq!(indices, (0..EACH).collect::<Vec<_>>());
        }
    }

    #[test]
    fn a_message_still_being_written_holds_back_the_ones_after_it() {
        let log = LogQueue::new();
        // A writer has claimed the first slot and taken the first number,
        // and has not finished.
        let slot = &log.slots[0];
        slot.state.store(WRITING, Ordering::Relaxed);
        let order = log.next.fetch_add(1, Ordering::Relaxed);
        log.post(LogLevel::Info, "later");
        assert_eq!(log.drain().records, [], "held back, not lost");
        // It finishes.
        slot.level.store(LogLevel::Warning as u8, Ordering::Relaxed);
        slot.length.store(7, Ordering::Relaxed);
        slot.order.store(order, Ordering::Relaxed);
        slot.words[0].store(u64::from_le_bytes(*b"earlier\0"), Ordering::Relaxed);
        slot.state.store(FULL, Ordering::Release);
        let texts: Vec<String> = log.drain().records.into_iter().map(|r| r.text).collect();
        assert_eq!(texts, ["earlier", "later"]);
    }

    #[test]
    fn a_queue_can_be_a_static() {
        static LOG: LogQueue = LogQueue::new();
        assert!(LOG.post(LogLevel::Info, "static"));
        assert_eq!(LOG.drain().records[0].text, "static");
    }
}
