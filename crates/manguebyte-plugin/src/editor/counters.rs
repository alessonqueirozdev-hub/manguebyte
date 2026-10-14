//! Gestures carried to the audio thread as counts.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering, fence};

/// How many gestures have begun and ended on each parameter, shared between
/// the main thread, where gestures happen, and the thread that tells the
/// host about them.
///
/// CLAP reports gestures as events that a plugin emits from `process` or
/// `flush`, so an edit made on the main thread has to reach the audio
/// thread. It travels as two counters per parameter, one of the two bridges
/// of ADR 0002: there is nothing to lock, nothing that fills up, and no
/// queue end to hand from one editor to the next. A wrapper's
/// [`EditorHost`](super::EditorHost) calls [`begin`](Self::begin) and
/// [`end`](Self::end); on the audio side a [`GestureReader`] turns the
/// counts into begin and end events.
///
/// Parameters are addressed by index, in the order the plugin's parameter
/// set visits them, which is the order hosts number them in.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_plugin::editor::{GestureCounters, GestureEdges, GestureReader};
///
/// let counters = Arc::new(GestureCounters::new(2));
/// let mut reader = GestureReader::new(Arc::clone(&counters));
///
/// // Main thread: a drag on parameter 1 starts, and a click on parameter 0
/// // comes and goes before the audio thread looks.
/// counters.begin(1);
/// counters.begin(0);
/// counters.end(0);
///
/// // Audio thread, in `process`.
/// assert_eq!(reader.read(0), GestureEdges { begin: true, end: true });
/// assert_eq!(reader.read(1), GestureEdges { begin: true, end: false });
///
/// // The drag ends.
/// counters.end(1);
/// assert_eq!(reader.read(1), GestureEdges { begin: false, end: true });
/// ```
pub struct GestureCounters {
    cells: Box<[Counts]>,
}

struct Counts {
    begun: AtomicU32,
    ended: AtomicU32,
}

impl GestureCounters {
    /// Counters for `len` parameters, all at zero. Allocates.
    pub fn new(len: usize) -> Self {
        let cells = (0..len)
            .map(|_| Counts {
                begun: AtomicU32::new(0),
                ended: AtomicU32::new(0),
            })
            .collect();
        Self { cells }
    }

    /// How many parameters are counted.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// Whether no parameter is counted.
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Counts a gesture beginning on parameter `index`. An index out of
    /// range is ignored. Real-time safe.
    ///
    /// Store the parameter's new values after this returns: a reader that
    /// sees such a value through [`GestureReader::read_with`] also sees
    /// this begin.
    pub fn begin(&self, index: usize) {
        if let Some(cell) = self.cells.get(index) {
            // Release: a reader that sees the matching end sees this too.
            cell.begun.fetch_add(1, Ordering::Release);
            // Orders the plain stores that follow, such as the parameter's
            // value, after this count; `read_with` pairs it with an acquire
            // fence.
            fence(Ordering::Release);
        }
    }

    /// Counts a gesture ending on parameter `index`. An end with no gesture
    /// open on that parameter is ignored, and so is an index out of range.
    /// Real-time safe.
    pub fn end(&self, index: usize) {
        let Some(cell) = self.cells.get(index) else {
            return;
        };
        let mut ended = cell.ended.load(Ordering::Relaxed);
        loop {
            // Begins only grow, so a gesture seen open here stays open
            // until this end is counted, whoever else is ending gestures.
            if cell.begun.load(Ordering::Acquire) == ended {
                return;
            }
            match cell.ended.compare_exchange_weak(
                ended,
                ended.wrapping_add(1),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(now) => ended = now,
            }
        }
    }

    /// Whether a gesture is open on parameter `index` right now.
    pub fn is_open(&self, index: usize) -> bool {
        self.cells.get(index).is_some_and(|cell| {
            let ended = cell.ended.load(Ordering::Acquire);
            cell.begun.load(Ordering::Acquire) != ended
        })
    }
}

impl fmt::Debug for GestureCounters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.cells.iter().map(|cell| {
                (
                    cell.begun.load(Ordering::Relaxed),
                    cell.ended.load(Ordering::Relaxed),
                )
            }))
            .finish()
    }
}

/// What happened to one parameter's gestures since a [`GestureReader`]
/// last looked, as the events to send a host.
///
/// Send them in this order: the begin, then the parameter's value if it
/// changed, then the end. [`GestureReader::read_with`] gives the value to
/// send with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct GestureEdges {
    /// A gesture began: send a gesture-begin event.
    pub begin: bool,
    /// The gesture ended: send a gesture-end event.
    pub end: bool,
}

/// Turns [`GestureCounters`] into the begin and end events a host is sent.
///
/// The reader owns a share of the counters it reads, so it cannot be given
/// others by mistake. It lives with the code that tells the host, on the
/// audio thread (or wherever CLAP's `flush` runs), and remembers per
/// parameter whether it has told the host a gesture is open. The events it
/// produces are always well formed: begin and end alternate on each
/// parameter, starting with a begin.
///
/// Gestures shorter than the time between two reads are not lost: one that
/// began and ended in between is reported as a begin and an end. Gestures
/// that touch between two reads merge, as the host would see them anyway: if
/// a gesture was open, and in between it ended and another began, the host
/// sees one gesture continue.
pub struct GestureReader {
    counters: Arc<GestureCounters>,
    /// The begin count at the last read, per parameter.
    seen: Box<[u32]>,
    /// Whether the host has been told of a gesture it has not seen end.
    open: Box<[bool]>,
}

impl GestureReader {
    /// A reader for `counters`, with nothing reported yet. A gesture
    /// already open is reported by the first read. Allocates.
    pub fn new(counters: Arc<GestureCounters>) -> Self {
        let seen = counters
            .cells
            .iter()
            .map(|cell| cell.begun.load(Ordering::Acquire))
            .collect();
        let open = vec![false; counters.len()].into_boxed_slice();
        Self {
            counters,
            seen,
            open,
        }
    }

    /// The counters this reader reads.
    pub fn counters(&self) -> &Arc<GestureCounters> {
        &self.counters
    }

    /// The events to send for parameter `index`. An index out of range
    /// gives no events.
    ///
    /// Real-time safe: two atomic loads and no allocation.
    pub fn read(&mut self, index: usize) -> GestureEdges {
        let (Some(cell), Some(seen), Some(open)) = (
            self.counters.cells.get(index),
            self.seen.get_mut(index),
            self.open.get_mut(index),
        ) else {
            return GestureEdges::default();
        };
        // The end count first: a begin always happens before its end, so
        // with acquire loads in this order the begins seen include the
        // begin of every end seen.
        let ended = cell.ended.load(Ordering::Acquire);
        let begun = cell.begun.load(Ordering::Acquire);
        let began = begun != *seen;
        let open_now = begun != ended;
        *seen = begun;

        let begin = !*open && (began || open_now);
        let end = (*open || begin) && !open_now;
        *open = open_now;
        GestureEdges { begin, end }
    }

    /// The events to send for parameter `index`, with the value to send
    /// between them, which `value` loads from the parameter.
    ///
    /// A value set during a gesture always lands inside it: a value stored
    /// after a begin was counted is never returned with edges that leave
    /// the host without an open gesture, and the value returned with an
    /// end is at least as recent as every value stored before that end was
    /// counted. To get that, `value` is called before the counters are read
    /// and, if they show a gesture, once more after.
    ///
    /// Real-time safe if `value` is.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicU64, Ordering};
    /// use manguebyte_plugin::editor::{GestureCounters, GestureEdges, GestureReader};
    ///
    /// // A parameter's normalized value, as the bits of an `f64`.
    /// let cutoff = Arc::new(AtomicU64::new(0.25_f64.to_bits()));
    /// let load = || f64::from_bits(cutoff.load(Ordering::Relaxed));
    /// let counters = Arc::new(GestureCounters::new(1));
    /// let mut reader = GestureReader::new(Arc::clone(&counters));
    ///
    /// // Main thread: a whole drag between two blocks.
    /// counters.begin(0);
    /// cutoff.store(0.75_f64.to_bits(), Ordering::Relaxed);
    /// counters.end(0);
    ///
    /// // Audio thread: send the begin, 0.75, then the end.
    /// let (edges, value) = reader.read_with(0, load);
    /// assert_eq!(edges, GestureEdges { begin: true, end: true });
    /// assert_eq!(value, 0.75);
    /// ```
    pub fn read_with<T>(
        &mut self,
        index: usize,
        mut value: impl FnMut() -> T,
    ) -> (GestureEdges, T) {
        let before = value();
        // Pairs with the release fence in `GestureCounters::begin`: if
        // `before` is a value stored after a begin, the read below sees
        // that begin.
        fence(Ordering::Acquire);
        let edges = self.read(index);
        if edges.begin || edges.end || self.is_open(index) {
            // The read above acquired every end it saw, so this sees each
            // value stored before them. A value stored after a begin the
            // read did not see still lands inside the gesture reported.
            (edges, value())
        } else {
            // No gesture was counted, so `before` was not set by one.
            (edges, before)
        }
    }

    /// Whether the host has been told of a gesture on parameter `index`
    /// that it has not yet been told has ended.
    pub fn is_open(&self, index: usize) -> bool {
        self.open.get(index).copied().unwrap_or(false)
    }
}

impl fmt::Debug for GestureReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GestureReader")
            .field("open", &self.open)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: GestureEdges = GestureEdges {
        begin: false,
        end: false,
    };
    const BEGIN: GestureEdges = GestureEdges {
        begin: true,
        end: false,
    };
    const END: GestureEdges = GestureEdges {
        begin: false,
        end: true,
    };
    const BOTH: GestureEdges = GestureEdges {
        begin: true,
        end: true,
    };

    fn counters_and_reader(len: usize) -> (Arc<GestureCounters>, GestureReader) {
        let counters = Arc::new(GestureCounters::new(len));
        let reader = GestureReader::new(Arc::clone(&counters));
        (counters, reader)
    }

    #[test]
    fn each_case_gives_exactly_these_events() {
        let (counters, mut reader) = counters_and_reader(1);
        assert_eq!(reader.read(0), NONE);

        // Open across reads.
        counters.begin(0);
        assert_eq!(reader.read(0), BEGIN);
        assert_eq!(reader.read(0), NONE);
        counters.end(0);
        assert_eq!(reader.read(0), END);
        assert_eq!(reader.read(0), NONE);

        // Begun and ended between two reads, twice.
        counters.begin(0);
        counters.end(0);
        counters.begin(0);
        counters.end(0);
        assert_eq!(reader.read(0), BOTH);
        assert!(!reader.is_open(0));

        // Open, then it ends and another begins between two reads: one
        // gesture goes on.
        counters.begin(0);
        assert_eq!(reader.read(0), BEGIN);
        counters.end(0);
        counters.begin(0);
        assert_eq!(reader.read(0), NONE);
        assert!(reader.is_open(0));
        counters.end(0);
        assert_eq!(reader.read(0), END);
    }

    #[test]
    fn a_gesture_open_when_the_reader_is_made_is_reported() {
        let counters = Arc::new(GestureCounters::new(1));
        counters.begin(0);
        let mut reader = GestureReader::new(Arc::clone(&counters));
        assert_eq!(reader.read(0), BEGIN);
        counters.end(0);
        assert_eq!(reader.read(0), END);
    }

    #[test]
    fn indices_out_of_range_are_harmless() {
        let (counters, mut reader) = counters_and_reader(2);
        counters.begin(7);
        counters.end(usize::MAX);
        assert!(!counters.is_open(7));
        assert_eq!(reader.read(2), NONE);
        assert_eq!(reader.read(usize::MAX), NONE);
        assert!(!reader.is_open(2));
        assert!(GestureCounters::new(0).is_empty());
        assert_eq!(counters.len(), 2);
    }

    /// A reader made for one set of counters reads only those: another set
    /// of the same length, with a gesture on it, gives no events and leaves
    /// the reader as it was.
    #[test]
    fn a_reader_reads_only_the_counters_it_was_made_for() {
        let (mine, mut reader) = counters_and_reader(2);
        let other = GestureCounters::new(2);
        other.begin(0);
        assert!(Arc::ptr_eq(reader.counters(), &mine));
        assert_eq!(reader.read(0), NONE);
        assert!(!reader.is_open(0));
        mine.begin(0);
        assert_eq!(reader.read(0), BEGIN);
    }

    /// An end with no gesture open (an editor or wrapper that ends a
    /// gesture twice, or one it never began) is not counted, so it can
    /// neither open a gesture at the host nor close one that begins later.
    #[test]
    fn an_unmatched_end_is_ignored() {
        let (counters, mut reader) = counters_and_reader(1);
        counters.end(0);
        assert_eq!(reader.read(0), NONE, "a stray end produced an event");
        assert!(!counters.is_open(0), "a stray end opened a gesture");

        // A gesture that ends twice: the second end changes nothing, and a
        // later gesture is still reported whole.
        counters.begin(0);
        counters.end(0);
        counters.end(0);
        assert_eq!(reader.read(0), BOTH);
        counters.begin(0);
        assert!(counters.is_open(0));
        assert_eq!(reader.read(0), BEGIN);
        counters.end(0);
        assert_eq!(reader.read(0), END);
        assert_eq!(format!("{counters:?}"), "[(2, 2)]");
    }

    #[test]
    fn counts_survive_wrapping_around() {
        let counters = Arc::new(GestureCounters::new(1));
        counters.cells[0].begun.store(u32::MAX, Ordering::Relaxed);
        counters.cells[0].ended.store(u32::MAX, Ordering::Relaxed);
        let mut reader = GestureReader::new(Arc::clone(&counters));
        counters.end(0); // nothing open: ignored
        counters.begin(0);
        assert_eq!(reader.read(0), BEGIN);
        counters.end(0);
        assert_eq!(reader.read(0), END);
        assert_eq!(counters.cells[0].begun.load(Ordering::Relaxed), 0);
        assert_eq!(counters.cells[0].ended.load(Ordering::Relaxed), 0);
    }

    /// Any interleaving of gestures (nested ones too), stray ends and reads
    /// gives events that alternate, a reader that agrees with the counters
    /// after every read, and a begin for every stretch in which a gesture
    /// was open.
    #[test]
    fn random_interleavings_give_well_formed_events() {
        const PARAMS: usize = 3;
        let mut random = 0x0123_4567_89AB_CDEF_u64;
        let mut next = move || {
            random ^= random >> 12;
            random ^= random << 25;
            random ^= random >> 27;
            random.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let rounds = if cfg!(miri) { 20 } else { 2_000 };
        for _ in 0..rounds {
            let (counters, mut reader) = counters_and_reader(PARAMS);
            let mut depth = [0_u32; PARAMS];
            let mut host_open = [false; PARAMS];
            let mut opened_since_read = [false; PARAMS];
            let (mut begun, mut reported) = (0, 0);
            for _ in 0..60 {
                let index = (next() % PARAMS as u64) as usize;
                match next() % 3 {
                    0 => {
                        counters.begin(index);
                        depth[index] += 1;
                        begun += 1;
                        opened_since_read[index] = true;
                    }
                    1 => {
                        // Sometimes with nothing open: then it is ignored.
                        counters.end(index);
                        depth[index] = depth[index].saturating_sub(1);
                    }
                    _ => {
                        for index in 0..PARAMS {
                            let edges = reader.read(index);
                            if edges.begin {
                                assert!(!host_open[index], "begin while open");
                                host_open[index] = true;
                                reported += 1;
                            }
                            if edges.end {
                                assert!(host_open[index], "end while closed");
                                host_open[index] = false;
                            }
                            assert_eq!(host_open[index], depth[index] > 0);
                            assert_eq!(counters.is_open(index), depth[index] > 0);
                            assert_eq!(reader.is_open(index), host_open[index]);
                            if opened_since_read[index] {
                                assert!(edges.begin || host_open[index] || edges.end);
                            }
                            opened_since_read[index] = false;
                        }
                    }
                }
            }
            for (index, open) in depth.iter_mut().enumerate() {
                while *open > 0 {
                    counters.end(index);
                    *open -= 1;
                }
                let edges = reader.read(index);
                assert!(!edges.begin || edges.end);
                assert!(!reader.is_open(index));
                reported += usize::from(edges.begin);
            }
            // Merging can only make fewer gestures, never more.
            assert!(reported <= begun);
        }
    }

    /// The main thread gestures while the audio thread reads: every begin
    /// the reader reports is matched by an end, and nothing is reported
    /// twice.
    #[test]
    fn a_writer_thread_and_a_reader_thread_agree() {
        use std::sync::atomic::AtomicBool;

        const PARAMS: usize = 4;
        let gestures: u32 = if cfg!(miri) { 50 } else { 20_000 };
        let (counters, mut reader) = counters_and_reader(PARAMS);
        let done = Arc::new(AtomicBool::new(false));

        let writer = {
            let counters = Arc::clone(&counters);
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                for gesture in 0..gestures {
                    let index = gesture as usize % PARAMS;
                    counters.begin(index);
                    counters.end(index);
                    counters.end(index); // a stray end, ignored
                }
                done.store(true, Ordering::Release);
            })
        };

        let mut open = [false; PARAMS];
        let mut begins = [0_u32; PARAMS];
        let mut read_all = |reader: &mut GestureReader| {
            for index in 0..PARAMS {
                let edges = reader.read(index);
                if edges.begin {
                    assert!(!open[index]);
                    open[index] = true;
                    begins[index] += 1;
                }
                if edges.end {
                    assert!(open[index]);
                    open[index] = false;
                }
            }
        };
        while !done.load(Ordering::Acquire) {
            read_all(&mut reader);
        }
        writer.join().expect("the writer thread panicked");
        read_all(&mut reader);

        assert_eq!(open, [false; PARAMS], "every gesture reported ended");
        for count in begins {
            assert!((1..=gestures / PARAMS as u32).contains(&count));
        }
        for index in 0..PARAMS {
            assert!(!counters.is_open(index));
        }
    }

    /// Every way two whole drags (begin, store a value, end) can fall
    /// around three reads, including between `read_with`'s first value
    /// load and its look at the counters, and between that look and its
    /// second load. The host is never sent a value outside a gesture, and
    /// ends up with the last value and no gesture open.
    #[test]
    fn every_interleaving_of_two_drags_and_three_reads_keeps_values_inside_gestures() {
        use std::cell::Cell;

        #[derive(Clone, Copy)]
        enum Step {
            Begin,
            Store(u32),
            End,
        }
        const STEPS: [Step; 6] = [
            Step::Begin,
            Step::Store(1),
            Step::End,
            Step::Begin,
            Step::Store(2),
            Step::End,
        ];
        const READS: usize = 3;
        // Three gaps per read: before it, after its first load, and before
        // its second load; then one after the last read.
        const SLOTS: usize = 3 * READS + 1;

        /// Runs every step scheduled for `slot` or earlier.
        fn run_to(
            slot: usize,
            schedule: &[usize],
            next: &mut usize,
            counters: &GestureCounters,
            value: &Cell<u32>,
        ) {
            while *next < STEPS.len() && schedule[*next] <= slot {
                match STEPS[*next] {
                    Step::Begin => counters.begin(0),
                    Step::Store(stored) => value.set(stored),
                    Step::End => counters.end(0),
                }
                *next += 1;
            }
        }

        let mut schedule = [0_usize; STEPS.len()];
        let mut interleavings = 0;
        loop {
            let (counters, mut reader) = counters_and_reader(1);
            let value = Cell::new(0_u32);
            let mut next = 0;
            let (mut open, mut sent) = (false, 0);
            for read in 0..=READS {
                let base = 3 * read;
                run_to(base, &schedule, &mut next, &counters, &value);
                // The last read comes after every step.
                let mut loads = 0;
                let (edges, now) = reader.read_with(0, || {
                    loads += 1;
                    if loads == 1 {
                        let before = value.get();
                        run_to(base + 1, &schedule, &mut next, &counters, &value);
                        before
                    } else {
                        run_to(base + 2, &schedule, &mut next, &counters, &value);
                        value.get()
                    }
                });
                if edges.begin {
                    assert!(!open, "{schedule:?}: begin while open");
                    open = true;
                }
                if now != sent {
                    assert!(open, "{schedule:?}: {now} sent with no gesture open");
                    sent = now;
                }
                if edges.end {
                    assert!(open, "{schedule:?}: end while closed");
                    open = false;
                }
            }
            assert_eq!(next, STEPS.len());
            assert!(!open, "{schedule:?}: a gesture was left open");
            assert_eq!(sent, 2, "{schedule:?}: the last value never arrived");
            interleavings += 1;

            // The next schedule: non-decreasing slots, in order.
            let Some(last) = (0..STEPS.len()).rev().find(|&i| schedule[i] < SLOTS - 1) else {
                break;
            };
            schedule[last] += 1;
            let slot = schedule[last];
            for later in &mut schedule[last + 1..] {
                *later = slot;
            }
        }
        // Six steps in ten slots, keeping their order: C(15, 6).
        assert_eq!(interleavings, 5005);
    }

    /// The main thread drags a parameter through rising values, one gesture
    /// per value, while the audio thread reads with `read_with` as a CLAP
    /// wrapper does. The host must never be sent a value outside a
    /// gesture, must see the values in order, and must end up with the
    /// last one.
    #[test]
    fn values_read_with_the_counters_always_land_inside_a_gesture() {
        use std::sync::atomic::{AtomicBool, AtomicU32};

        let gestures: u32 = if cfg!(miri) { 40 } else { 50_000 };
        let (counters, mut reader) = counters_and_reader(1);
        let value = Arc::new(AtomicU32::new(0));
        let done = Arc::new(AtomicBool::new(false));

        let writer = {
            let (counters, value, done) =
                (Arc::clone(&counters), Arc::clone(&value), Arc::clone(&done));
            std::thread::spawn(move || {
                for step in 1..=gestures {
                    counters.begin(0);
                    // Stored the way parameters store their values.
                    value.store(step, Ordering::Relaxed);
                    counters.end(0);
                }
                done.store(true, Ordering::Release);
            })
        };

        let mut open = false;
        let mut sent = 0;
        let mut read = |reader: &mut GestureReader| {
            let (edges, now) = reader.read_with(0, || value.load(Ordering::Relaxed));
            if edges.begin {
                assert!(!open);
                open = true;
            }
            if now != sent {
                assert!(open, "{now} was sent with no gesture open");
                assert!(now > sent, "{now} was sent after {sent}");
                sent = now;
            }
            if edges.end {
                assert!(open);
                open = false;
            }
        };
        while !done.load(Ordering::Acquire) {
            read(&mut reader);
        }
        writer.join().expect("the writer thread panicked");
        read(&mut reader);

        assert!(!open);
        assert_eq!(sent, gestures, "the last value reached the host");
    }
}
