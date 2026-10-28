//! What showing a hosted plugin's editor has in common across formats.
//!
//! An editor lives inside a window the application owns. The application
//! hands the plugin a [`ParentWindow`], and the plugin puts its editor in
//! it; what the editor draws, and how, is the plugin's business. The editor
//! then asks for things the plugin cannot do alone: a new size for the
//! window around it, to be shown or hidden, and time to run in, as timers.
//!
//! This crate has no window and no event loop of its own, so it cannot
//! answer most of those requests by itself. It passes them on, as
//! [`EditorRequest`]s among the events each format's `pump` returns, and
//! the application carries them out with its own windows. Timers are the
//! exception: the plugin's main half keeps them, fires the ones that are
//! due whenever the application calls its `pump`, and says when the next
//! one is due, so the application knows when to call again.
//!
//! [`ParentWindow`] and [`WindowApi`] are the types a plugin made with this
//! framework receives on the other side, so both ends name windows alike.

use std::time::{Duration, Instant};

pub use manguebyte_plugin::editor::{ParentWindow, WindowApi};

/// The size of an editor, in the pixels its format counts in: physical
/// pixels on Windows and X11, points on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EditorSize {
    /// The width.
    pub width: u32,
    /// The height.
    pub height: u32,
}

impl EditorSize {
    /// A size of `width` by `height`.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// Something a hosted plugin's editor asked the application for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EditorRequest {
    /// The editor wants to be this size. Make the parent window that size,
    /// then tell the plugin with its format's `set_editor_size`; until
    /// then, the editor keeps its old size.
    Resize(EditorSize),
    /// The editor asks to be shown, for example because the person asked
    /// for it from somewhere else in the plugin.
    Show,
    /// The editor asks to be hidden.
    Hide,
    /// The plugin closed its editor by itself. Nothing of it is left to
    /// close; the parent window can go.
    Closed,
}

/// The fewest milliseconds between two firings of a timer. A plugin that
/// asks for less, zero included, gets this.
pub(crate) const MIN_TIMER_PERIOD: Duration = Duration::from_millis(1);

/// The most timers one plugin can hold at once. A plugin that asks for
/// more is refused, rather than let it grow the list without end.
pub(crate) const MAX_TIMERS: usize = 64;

/// The timers a plugin's editor registered, each with the next time it is
/// due. Only the plugin's main thread touches this.
#[derive(Debug, Default)]
pub(crate) struct Timers {
    entries: Vec<Timer>,
    /// The id the next timer gets. Ids are not reused while the plugin
    /// lives, so a stale id never stops someone else's timer.
    next_id: u32,
}

#[derive(Debug, Clone, Copy)]
struct Timer {
    id: u32,
    period: Duration,
    due: Instant,
}

impl Timers {
    /// Registers a timer that fires every `period`, first at
    /// `now + period`, and returns its id; `None` if the plugin already
    /// holds [`MAX_TIMERS`] or has used up every id.
    pub(crate) fn add(&mut self, period: Duration, now: Instant) -> Option<u32> {
        if self.entries.len() >= MAX_TIMERS {
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1)?;
        let period = period.max(MIN_TIMER_PERIOD);
        self.entries.push(Timer {
            id,
            period,
            due: now + period,
        });
        Some(id)
    }

    /// Forgets the timer `id`; false if there is none.
    pub(crate) fn remove(&mut self, id: u32) -> bool {
        let before = self.entries.len();
        self.entries.retain(|timer| timer.id != id);
        self.entries.len() != before
    }

    /// Forgets every timer, as when the editor closes.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    /// Whether any timer is registered.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// When the earliest timer is next due.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.entries.iter().map(|timer| timer.due).min()
    }

    /// Puts the ids of the timers due at `now` in `due`, in the order they
    /// were registered, and moves each one's next time on. A timer fires
    /// at most once per call, however late the call is.
    pub(crate) fn take_due(&mut self, now: Instant, due: &mut Vec<u32>) {
        due.clear();
        for timer in &mut self.entries {
            if timer.due > now {
                continue;
            }
            due.push(timer.id);
            timer.due = next_time(timer.due, timer.period, now);
        }
    }
}

/// When a timer that was due at `due`, fires every `period`, and is being
/// fired at `now` (`now >= due`) is next due. The result is after `now`.
///
/// The ticks missed while the application was busy are skipped, not made
/// up for, and the timer keeps its phase: an editor that draws every 16 ms
/// gains nothing from drawing the frames it missed, and a burst of them
/// would only delay the application further.
fn next_time(due: Instant, period: Duration, now: Instant) -> Instant {
    let late = now.saturating_duration_since(due).as_nanos();
    // `period` is at least `MIN_TIMER_PERIOD`, so the division is defined.
    let missed = late / period.as_nanos();
    // A count that does not fit in `u32` is a timer late by years; starting
    // it again one period from now is as good as anything.
    u32::try_from(missed + 1)
        .ok()
        .and_then(|ticks| period.checked_mul(ticks))
        .and_then(|ahead| due.checked_add(ahead))
        .unwrap_or(now + period)
}

/// What an editor waits for on a file descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct FdInterest {
    /// The descriptor has something to read.
    pub read: bool,
    /// The descriptor can be written to.
    pub write: bool,
    /// Something went wrong with the descriptor.
    pub error: bool,
}

impl FdInterest {
    /// Whether nothing at all is asked for.
    pub const fn is_empty(self) -> bool {
        !self.read && !self.write && !self.error
    }
}

/// A file descriptor a hosted plugin's editor asked to be told about, on
/// systems that have them. The application watches it with its own loop
/// (`poll`, its toolkit's watches) and tells the plugin when it is ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FdWatch {
    /// The descriptor.
    pub fd: i32,
    /// What the editor waits for on it.
    pub interest: FdInterest,
}

/// The most descriptors one plugin can have watched at once.
pub(crate) const MAX_FD_WATCHES: usize = 64;

/// The descriptors a plugin's editor asked to be watched. Only the
/// plugin's main thread touches this.
#[derive(Debug, Default)]
pub(crate) struct FdWatches {
    entries: Vec<FdWatch>,
}

impl FdWatches {
    /// Starts watching `fd`; false if it is already watched, is negative,
    /// asks for nothing, or the plugin already has [`MAX_FD_WATCHES`].
    pub(crate) fn add(&mut self, fd: i32, interest: FdInterest) -> bool {
        if fd < 0 || interest.is_empty() || self.entries.len() >= MAX_FD_WATCHES {
            return false;
        }
        if self.entries.iter().any(|watch| watch.fd == fd) {
            return false;
        }
        self.entries.push(FdWatch { fd, interest });
        true
    }

    /// Changes what is waited for on `fd`; false if it is not watched or
    /// the new interest is empty.
    // Only CLAP's `posix-fd-support` changes a watch; VST 3 has no call
    // for it, so a build that hosts VST 3 alone has no use for this.
    #[cfg_attr(not(feature = "clap"), allow(dead_code))]
    pub(crate) fn modify(&mut self, fd: i32, interest: FdInterest) -> bool {
        if interest.is_empty() {
            return false;
        }
        match self.entries.iter_mut().find(|watch| watch.fd == fd) {
            Some(watch) => {
                watch.interest = interest;
                true
            }
            None => false,
        }
    }

    /// Stops watching `fd`; false if it was not watched.
    pub(crate) fn remove(&mut self, fd: i32) -> bool {
        let before = self.entries.len();
        self.entries.retain(|watch| watch.fd != fd);
        self.entries.len() != before
    }

    /// What `fd` is watched for, if it is.
    pub(crate) fn get(&self, fd: i32) -> Option<FdInterest> {
        self.entries
            .iter()
            .find(|watch| watch.fd == fd)
            .map(|watch| watch.interest)
    }

    /// Every watched descriptor, in the order they were added.
    pub(crate) fn list(&self) -> &[FdWatch] {
        &self.entries
    }

    /// Stops watching everything, as when the editor closes.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timers_fire_when_due_and_not_before() {
        let start = Instant::now();
        let mut timers = Timers::default();
        let fast = timers.add(Duration::from_millis(10), start).unwrap();
        let slow = timers.add(Duration::from_millis(30), start).unwrap();
        assert_eq!(timers.next_due(), Some(start + Duration::from_millis(10)));

        let mut due = Vec::new();
        timers.take_due(start + Duration::from_millis(9), &mut due);
        assert!(due.is_empty());
        timers.take_due(start + Duration::from_millis(10), &mut due);
        assert_eq!(due, [fast]);
        timers.take_due(start + Duration::from_millis(30), &mut due);
        assert_eq!(due, [fast, slow]);
        assert!(timers.next_due().unwrap() > start + Duration::from_millis(30));
    }

    #[test]
    fn a_late_call_fires_each_timer_once_and_leaves_it_due_later() {
        let start = Instant::now();
        let mut timers = Timers::default();
        let id = timers.add(Duration::from_millis(10), start).unwrap();
        let late = start + Duration::from_millis(95);
        let mut due = Vec::new();
        timers.take_due(late, &mut due);
        assert_eq!(due, [id]);
        assert!(timers.next_due().unwrap() > late);
        timers.take_due(late, &mut due);
        assert!(due.is_empty());
    }

    #[test]
    fn a_late_timer_skips_the_ticks_it_missed_and_keeps_its_phase() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        // Due at 10 ms, fired at 95 ms: the ticks at 20 to 90 ms are
        // skipped, and the next is at 100 ms, not at 105 ms.
        assert_eq!(
            next_time(start + ms(10), ms(10), start + ms(95)),
            start + ms(100)
        );
        // Fired exactly on a tick, the next is one period on.
        assert_eq!(
            next_time(start + ms(10), ms(10), start + ms(30)),
            start + ms(40)
        );
        assert_eq!(
            next_time(start + ms(10), ms(10), start + ms(10)),
            start + ms(20)
        );
    }

    #[test]
    fn removed_timers_stop_and_ids_are_not_reused() {
        let start = Instant::now();
        let mut timers = Timers::default();
        let first = timers.add(Duration::ZERO, start).unwrap();
        assert!(timers.remove(first));
        assert!(!timers.remove(first));
        assert!(timers.is_empty());
        let second = timers.add(Duration::from_millis(5), start).unwrap();
        assert_ne!(first, second);
        let mut due = Vec::new();
        timers.take_due(start + Duration::from_secs(1), &mut due);
        assert_eq!(due, [second]);
    }

    #[test]
    fn a_zero_period_is_held_to_the_shortest() {
        let start = Instant::now();
        let mut timers = Timers::default();
        timers.add(Duration::ZERO, start).unwrap();
        assert_eq!(timers.next_due(), Some(start + MIN_TIMER_PERIOD));
    }

    #[test]
    fn a_plugin_cannot_hold_more_than_the_most_timers() {
        let start = Instant::now();
        let mut timers = Timers::default();
        for _ in 0..MAX_TIMERS {
            assert!(timers.add(Duration::from_millis(16), start).is_some());
        }
        assert_eq!(timers.add(Duration::from_millis(16), start), None);
        timers.clear();
        assert!(timers.add(Duration::from_millis(16), start).is_some());
    }

    #[test]
    fn descriptors_are_watched_once_and_can_be_changed_and_dropped() {
        let read = FdInterest {
            read: true,
            ..FdInterest::default()
        };
        let both = FdInterest {
            read: true,
            write: true,
            error: false,
        };
        let mut watches = FdWatches::default();
        assert!(watches.add(5, read));
        assert!(!watches.add(5, both), "already watched");
        assert!(!watches.add(-1, read), "no such descriptor");
        assert!(!watches.add(6, FdInterest::default()), "asks for nothing");
        assert!(watches.modify(5, both));
        assert_eq!(watches.get(5), Some(both));
        assert!(!watches.modify(7, both));
        assert!(!watches.modify(5, FdInterest::default()));
        assert_eq!(
            watches.list(),
            [FdWatch {
                fd: 5,
                interest: both
            }]
        );
        assert!(watches.remove(5));
        assert!(!watches.remove(5));
        for fd in 0..MAX_FD_WATCHES {
            assert!(watches.add(i32::try_from(fd).unwrap(), read));
        }
        assert!(!watches.add(1_000, read));
        watches.clear();
        assert!(watches.list().is_empty());
    }
}
