//! What a running stream's thread decides each time the driver signals it:
//! how many packets to read, what a period without a buffer means, and when
//! to stop. Nothing here touches a device, so it is tested on every system.
//! It runs on the stream's thread, and allocates only on the way to stopping
//! a stream.

// Only Windows' module calls these outside the tests.
#![cfg_attr(not(windows), allow(dead_code))]

use std::panic::{self, AssertUnwindSafe};

use crate::Error;
use crate::negotiate::{code, error_for};

/// The most packets read after one of the driver's events. Exclusive mode
/// holds about a period, so only a driver that never says it has no more
/// gets this far.
pub(crate) const MOST_PACKETS: u32 = 8;

/// How one period went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Period {
    /// Audio moved.
    Served,
    /// There was nothing to move.
    Idle,
    /// The driver had no buffer for it
    /// ([`no_buffer`](crate::negotiate::no_buffer)).
    Missed,
}

/// What one `GetBuffer` of a capture stream came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Taken {
    /// A packet, read and handed back.
    Packet,
    /// There is none left.
    Empty,
    /// The driver had no buffer to give.
    NoBuffer,
}

/// Reads the packets there are: calls `take` until there is none left, at
/// most [`MOST_PACKETS`] times. The period is [`Period::Missed`] when the
/// first call finds no buffer; after a packet, that only says there is no
/// more.
pub(crate) fn drain(mut take: impl FnMut() -> Result<Taken, Error>) -> Result<Period, Error> {
    let mut period = Period::Idle;
    for _ in 0..MOST_PACKETS {
        match take()? {
            Taken::Packet => period = Period::Served,
            Taken::NoBuffer if period == Period::Idle => return Ok(Period::Missed),
            Taken::Empty | Taken::NoBuffer => break,
        }
    }
    Ok(period)
}

/// Runs the periods of one stream, and says when it has to stop.
#[derive(Debug)]
pub(crate) struct Watch {
    /// Periods missed since audio last moved.
    missed: u32,
    /// How many of them end the stream: about a second's worth.
    limit: u32,
}

impl Watch {
    /// For a stream with periods of `period_frames` at `sample_rate`.
    pub(crate) fn new(sample_rate: u32, period_frames: u32) -> Self {
        Self {
            missed: 0,
            limit: sample_rate.div_ceil(period_frames.max(1)).max(1),
        }
    }

    /// Runs `period`. An error means the stream stops, with it as the
    /// reason: the period's own, [`Error::CallbackPanicked`] when it
    /// panicked, or the driver's having had no buffer for about a second of
    /// periods in a row.
    pub(crate) fn run(
        &mut self,
        period: impl FnOnce() -> Result<Period, Error>,
    ) -> Result<(), Error> {
        // After a panic the stream stops, and what the callback may have
        // left half done is not run again.
        let Ok(outcome) = panic::catch_unwind(AssertUnwindSafe(period)) else {
            return Err(Error::CallbackPanicked);
        };
        match outcome? {
            Period::Served => self.missed = 0,
            Period::Idle => {}
            Period::Missed => {
                self.missed = self.missed.saturating_add(1);
                if self.missed >= self.limit {
                    return Err(error_for(
                        code::BUFFER_ERROR,
                        "the driver had no buffer for a second",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `drain` makes of these answers, and how many it asked for.
    fn drained(answers: &[Taken]) -> (Period, usize) {
        let mut asked = 0;
        let period = drain(|| {
            let answer = answers.get(asked).copied().unwrap_or(Taken::Empty);
            asked += 1;
            Ok(answer)
        })
        .unwrap();
        (period, asked)
    }

    #[test]
    fn every_packet_there_is_gets_read() {
        use Taken::{Empty, NoBuffer, Packet};
        assert_eq!(drained(&[Empty]), (Period::Idle, 1));
        assert_eq!(drained(&[Packet, Empty]), (Period::Served, 2));
        assert_eq!(
            drained(&[Packet, Packet, Packet, Empty]),
            (Period::Served, 4)
        );
        // Exclusive mode may answer "no buffer" once the packet is read.
        assert_eq!(drained(&[Packet, NoBuffer]), (Period::Served, 2));
        assert_eq!(drained(&[NoBuffer, Packet]), (Period::Missed, 1));
    }

    #[test]
    fn a_driver_that_never_runs_out_is_left_after_a_few_packets() {
        let (period, asked) = drained(&[Taken::Packet; 100]);
        assert_eq!((period, asked), (Period::Served, MOST_PACKETS as usize));
    }

    #[test]
    fn a_failed_read_ends_the_pass() {
        let mut asked = 0;
        let error = drain(|| {
            asked += 1;
            if asked == 1 {
                Ok(Taken::Packet)
            } else {
                Err(Error::DeviceLost)
            }
        })
        .unwrap_err();
        assert_eq!((error, asked), (Error::DeviceLost, 2));
    }

    #[test]
    fn about_a_second_without_a_buffer_ends_a_stream() {
        // 48 kHz in periods of 144 frames is 333.3 periods a second.
        let mut watch = Watch::new(48_000, 144);
        for _ in 0..333 {
            assert_eq!(watch.run(|| Ok(Period::Missed)), Ok(()));
        }
        let error = watch.run(|| Ok(Period::Missed)).unwrap_err();
        assert_eq!(
            error,
            Error::Os(String::from(
                "the driver had no buffer for a second (0x88890018)"
            ))
        );
        // Nonsense does not divide by zero, nor stop a stream that serves.
        for (rate, frames) in [(0, 0), (48_000, 0), (1, u32::MAX)] {
            let mut watch = Watch::new(rate, frames);
            assert_eq!(watch.run(|| Ok(Period::Served)), Ok(()));
        }
    }

    #[test]
    fn audio_that_moves_starts_the_count_again() {
        let mut watch = Watch::new(48_000, 12_000);
        for _ in 0..10 {
            for _ in 0..3 {
                assert_eq!(watch.run(|| Ok(Period::Missed)), Ok(()));
            }
            assert_eq!(watch.run(|| Ok(Period::Served)), Ok(()));
        }
        // An empty pass neither counts nor starts again.
        for _ in 0..3 {
            assert_eq!(watch.run(|| Ok(Period::Missed)), Ok(()));
            assert_eq!(watch.run(|| Ok(Period::Idle)), Ok(()));
        }
        assert!(watch.run(|| Ok(Period::Missed)).is_err());
    }

    #[test]
    fn an_error_or_a_panic_ends_a_stream() {
        let mut watch = Watch::new(48_000, 480);
        assert_eq!(watch.run(|| Err(Error::DeviceLost)), Err(Error::DeviceLost));
        assert_eq!(
            watch.run(|| panic!("a callback's panic")),
            Err(Error::CallbackPanicked)
        );
    }
}
