//! Delivers events to the handler one at a time.
//!
//! A window's handler may ask the window for things that make the system
//! send the window another message at once: a resize sends a size message,
//! taking the keyboard sends a focus message, a test sends input. That
//! message arrives while the handler is still running. Calling the handler
//! again then would borrow it twice, so the event is queued instead and
//! delivered as soon as the running call returns, in the order it came.
//! The handler is never re-entered.
//!
//! While the handler runs, a queued event that only repeats news already
//! in the queue is merged instead of queued again, so that a handler that
//! runs for a long time (a nested modal loop, a file dialog) does not come
//! back to a burst of stale events:
//!
//! - a timer tick is dropped when a tick of the same timer already waits;
//! - a pointer move replaces the last move that waits, when nothing but
//!   paints and timer ticks waits after it, so moves never cross a press,
//!   a release or a key;
//! - a paint merges its damage into a paint that waits, unless a resize or
//!   a scale change waits after it.
//!
//! Anything else is queued, up to [`MAX_QUEUED`] events. Beyond that new
//! events are dropped and counted
//! ([`Window::dropped_events`](crate::Window::dropped_events)).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};

use crate::WindowCx;
use crate::event::{Event, Handled};

/// The most events that wait for a running handler, after merging. More
/// are dropped and counted.
pub const MAX_QUEUED: usize = 1024;

/// What a window does with the events of the system.
///
/// The backend calls [`event`](Self::event) on the thread that owns the
/// window, one event at a time. The handler is given the window, so it can
/// present, invalidate, resize and set timers from inside the call;
/// anything those make the system send back arrives after the call
/// returns, never inside it.
///
/// A closure `FnMut(&WindowCx<'_>, Event) -> Handled` is a handler.
///
/// If the handler panics, the panic is caught at the window: the handler
/// is dropped, the window stops delivering events, and
/// [`Window::is_poisoned`](crate::Window::is_poisoned) says so. Nothing
/// unwinds into the host.
pub trait WindowHandler {
    /// Handles one event. The return value matters for keys, typed text
    /// and wheel turns: one the handler does not use goes on to the host's
    /// window (see [`Handled`]).
    fn event(&mut self, window: &WindowCx<'_>, event: Event) -> Handled;
}

impl<F> WindowHandler for F
where
    F: FnMut(&WindowCx<'_>, Event) -> Handled,
{
    fn event(&mut self, window: &WindowCx<'_>, event: Event) -> Handled {
        self(window, event)
    }
}

/// The system message an event came from, kept so that the event can go
/// on to the host if the handler does not use it. Plain numbers: the
/// Win32 backend makes them from its messages, and tests make them by
/// hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Origin {
    pub(crate) message: u32,
    pub(crate) wparam: usize,
    pub(crate) lparam: isize,
}

/// An event that waits, and the message it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Queued {
    event: Event,
    origin: Option<Origin>,
}

/// The handler and the events that wait for it.
pub(crate) struct Dispatcher {
    handler: RefCell<Option<Box<dyn WindowHandler>>>,
    queue: RefCell<VecDeque<Queued>>,
    busy: Cell<bool>,
    poisoned: Cell<bool>,
    /// The window was dropped: the handler goes as soon as it is free.
    retired: Cell<bool>,
    /// How many events arrived while the handler was running.
    deferred: Cell<u64>,
    /// How many of those were dropped because the queue was full.
    dropped: Cell<u64>,
}

/// Clears the busy flag however the dispatch ends.
struct Busy<'a>(&'a Cell<bool>);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl Dispatcher {
    pub(crate) fn new(handler: Box<dyn WindowHandler>) -> Self {
        Dispatcher {
            handler: RefCell::new(Some(handler)),
            queue: RefCell::new(VecDeque::new()),
            busy: Cell::new(false),
            poisoned: Cell::new(false),
            retired: Cell::new(false),
            deferred: Cell::new(0),
            dropped: Cell::new(0),
        }
    }

    /// Delivers `event`, or queues it if the handler is running. Returns
    /// what the handler said, or `None` if the event was queued or there
    /// is no handler to give it to.
    ///
    /// When the event has an `origin`, the window hears the handler's
    /// answer through [`WindowCx::answered`], whenever the event is
    /// delivered; a poisoned window answers [`Handled::No`] at once, so
    /// that what the handler can no longer use still reaches the host.
    pub(crate) fn dispatch(
        &self,
        window: &WindowCx<'_>,
        event: Event,
        origin: Option<Origin>,
    ) -> Option<Handled> {
        if self.retired.get() {
            return None;
        }
        if self.poisoned.get() {
            if let Some(origin) = origin {
                window.answered(origin, &event, Handled::No);
            }
            return None;
        }
        if self.busy.get() {
            self.deferred.set(self.deferred.get().saturating_add(1));
            self.enqueue(Queued { event, origin });
            return None;
        }
        let mut handler = self.handler.borrow_mut().take()?;
        self.busy.set(true);
        let busy = Busy(&self.busy);

        let first = self.call(&mut handler, window, Queued { event, origin });
        while !self.poisoned.get() {
            let next = self.queue.borrow_mut().pop_front();
            let Some(next) = next else { break };
            self.call(&mut handler, window, next);
        }
        drop(busy);

        if self.poisoned.get() || self.retired.get() {
            self.queue.borrow_mut().clear();
            drop_quietly(handler);
        } else {
            *self.handler.borrow_mut() = Some(handler);
        }
        first
    }

    /// Queues `queued`, merging it with what already waits where it only
    /// repeats news.
    fn enqueue(&self, queued: Queued) {
        let mut queue = self.queue.borrow_mut();
        if merge(&mut queue, &queued) {
            return;
        }
        if queue.len() >= MAX_QUEUED {
            self.dropped.set(self.dropped.get().saturating_add(1));
            return;
        }
        queue.push_back(queued);
    }

    fn call(
        &self,
        handler: &mut Box<dyn WindowHandler>,
        window: &WindowCx<'_>,
        queued: Queued,
    ) -> Option<Handled> {
        let Queued { event, origin } = queued;
        match panic::catch_unwind(AssertUnwindSafe(|| handler.event(window, event))) {
            Ok(handled) => {
                if let Some(origin) = origin {
                    window.answered(origin, &event, handled);
                }
                Some(handled)
            }
            Err(payload) => {
                self.poisoned.set(true);
                drop_quietly(payload);
                None
            }
        }
    }

    /// Stops all delivery; returns the handler if it is not running, for
    /// the caller to drop. A running handler is dropped when it returns.
    pub(crate) fn retire(&self) -> Option<Box<dyn WindowHandler>> {
        self.retired.set(true);
        self.queue.borrow_mut().clear();
        self.handler.borrow_mut().take()
    }

    /// Whether a handler panicked.
    pub(crate) fn poisoned(&self) -> bool {
        self.poisoned.get()
    }

    /// Whether the handler is running now.
    pub(crate) fn busy(&self) -> bool {
        self.busy.get()
    }

    /// How many events arrived while the handler was running.
    pub(crate) fn deferred(&self) -> u64 {
        self.deferred.get()
    }

    /// How many events were dropped because the queue was full.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.get()
    }

    /// How many events wait now, for tests.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.queue.borrow().len()
    }
}

/// Merges `new` into what waits in `queue`, if it only repeats news there.
/// Returns whether it did.
fn merge(queue: &mut VecDeque<Queued>, new: &Queued) -> bool {
    match new.event {
        Event::Timer(id) => queue
            .iter()
            .any(|waiting| waiting.event == Event::Timer(id)),
        Event::PointerMove(_) => {
            // The last move that waits, if no other input waits after it:
            // only paints and ticks may come between.
            for waiting in queue.iter_mut().rev() {
                match waiting.event {
                    Event::PointerMove(_) => {
                        waiting.event = new.event;
                        return true;
                    }
                    Event::Paint(_) | Event::Timer(_) => {}
                    _ => return false,
                }
            }
            false
        }
        Event::Paint(damage) => {
            for waiting in queue.iter_mut().rev() {
                match &mut waiting.event {
                    Event::Paint(kept) => {
                        kept.add_all(&damage);
                        return true;
                    }
                    // Damage after a resize or a scale change is for the
                    // new size; it must not be drawn before it.
                    Event::Resized(_) | Event::ScaleChanged(_) | Event::Closed => return false,
                    _ => {}
                }
            }
            false
        }
        _ => false,
    }
}

/// Drops `value`, catching a panic from its destructor.
pub(crate) fn drop_quietly<T>(value: T) {
    let _ = panic::catch_unwind(AssertUnwindSafe(move || drop(value)));
}

#[cfg(test)]
mod tests {
    use manguebyte_gfx::{IRect, Point};

    use super::*;
    use crate::damage::Damage;
    use crate::event::{PhysicalPoint, PhysicalSize, PointerEvent, TimerId};
    use crate::{Modifiers, Window, WindowOptions};

    fn moved_to(x: i32, y: i32) -> Event {
        Event::PointerMove(PointerEvent {
            position: Point::new(x as f32, y as f32),
            physical: PhysicalPoint::new(x, y),
            modifiers: Modifiers::NONE,
        })
    }

    fn moved(x: i32) -> Event {
        moved_to(x, 0)
    }

    fn queued(event: Event) -> Queued {
        Queued {
            event,
            origin: None,
        }
    }

    fn events(queue: &VecDeque<Queued>) -> Vec<Event> {
        queue.iter().map(|q| q.event).collect()
    }

    #[test]
    fn ticks_of_a_timer_that_already_waits_are_merged() {
        let mut queue = VecDeque::new();
        for event in [Event::Timer(TimerId(1)), Event::Focus(true)] {
            assert!(!merge(&mut queue, &queued(event)));
            queue.push_back(queued(event));
        }
        assert!(merge(&mut queue, &queued(Event::Timer(TimerId(1)))));
        assert!(!merge(&mut queue, &queued(Event::Timer(TimerId(2)))));
        assert_eq!(
            events(&queue),
            [Event::Timer(TimerId(1)), Event::Focus(true)]
        );
    }

    #[test]
    fn a_move_replaces_the_last_move_unless_other_input_waits_after_it() {
        let mut queue = VecDeque::new();
        queue.push_back(queued(moved(1)));
        assert!(merge(&mut queue, &queued(moved(2))));
        assert_eq!(events(&queue), [moved(2)]);
        // Paints and ticks may come between.
        queue.push_back(queued(Event::Timer(TimerId(1))));
        queue.push_back(queued(Event::Paint(Damage::of(IRect::new(0, 0, 1, 1)))));
        assert!(merge(&mut queue, &queued(moved(3))));
        assert_eq!(events(&queue)[0], moved(3));
        // A move after other input keeps its place in the order.
        queue.push_back(queued(Event::PointerLeave));
        assert!(!merge(&mut queue, &queued(moved(4))));
    }

    #[test]
    fn paints_merge_their_damage_but_not_across_a_resize() {
        let mut queue = VecDeque::new();
        queue.push_back(queued(Event::Paint(Damage::of(IRect::new(0, 0, 2, 2)))));
        queue.push_back(queued(Event::Focus(true)));
        assert!(merge(
            &mut queue,
            &queued(Event::Paint(Damage::of(IRect::new(10, 0, 2, 2))))
        ));
        let mut both = Damage::of(IRect::new(0, 0, 2, 2));
        both.add(IRect::new(10, 0, 2, 2));
        assert_eq!(events(&queue), [Event::Paint(both), Event::Focus(true)]);

        queue.push_back(queued(Event::Resized(PhysicalSize::new(50, 50))));
        let later = Event::Paint(Damage::of(IRect::new(40, 0, 10, 50)));
        assert!(!merge(&mut queue, &queued(later)));
    }

    /// A handler stuck in a nested loop while a 16 ms timer ticks and the
    /// pointer moves: the queue stays small, and nothing but merged
    /// repeats is lost.
    #[test]
    fn a_long_running_handler_comes_back_to_one_tick_and_one_move() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let seen: Rc<RefCell<Vec<Event>>> = Rc::default();
        let controller: Rc<RefCell<Option<crate::Headless>>> = Rc::default();
        let (window, headless) = {
            let (seen, controller) = (Rc::clone(&seen), Rc::clone(&controller));
            Window::headless(
                WindowOptions::new(PhysicalSize::new(100, 100)),
                move |window: &WindowCx<'_>, event| {
                    seen.borrow_mut().push(event);
                    if event == Event::Focus(true) {
                        let headless = controller.borrow().clone().expect("set");
                        window.set_timer(TimerId(7), 16).expect("open");
                        for i in 0..18_000 {
                            headless.input(crate::RawInput::Timer(TimerId(7)));
                            headless.input(crate::RawInput::PointerMove {
                                at: PhysicalPoint::new(i % 100, 5),
                                modifiers: Modifiers::NONE,
                            });
                            window.invalidate(IRect::new(i % 100, 0, 1, 1));
                            headless.expose();
                        }
                        let waiting = window.dispatcher_for_tests().queued();
                        assert!(waiting <= 3, "{waiting}");
                    }
                    Handled::Yes
                },
            )
            .expect("valid options")
        };
        *controller.borrow_mut() = Some(headless.clone());
        headless.input(crate::RawInput::Focus(true));
        let seen = seen.borrow();
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert_eq!(seen[0], Event::Focus(true));
        assert_eq!(seen[1], Event::Timer(TimerId(7)));
        assert_eq!(seen[2], moved_to(99, 5));
        let Event::Paint(damage) = seen[3] else {
            panic!("{:?}", seen[3])
        };
        // Every invalidated pixel is still covered.
        for x in 0..100 {
            assert!(
                damage.rects().iter().any(|r| r.contains(i64::from(x), 0)),
                "{x}"
            );
        }
        assert_eq!(window.deferred_events(), 3 * 18_000);
        assert_eq!(window.dropped_events(), 0);
    }

    #[test]
    fn a_full_queue_drops_and_counts() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let delivered = Rc::new(RefCell::new(0usize));
        let controller: Rc<RefCell<Option<crate::Headless>>> = Rc::default();
        let (window, headless) = {
            let (delivered, controller) = (Rc::clone(&delivered), Rc::clone(&controller));
            Window::headless(
                WindowOptions::new(PhysicalSize::new(10, 10)),
                move |_: &WindowCx<'_>, event| {
                    *delivered.borrow_mut() += 1;
                    if event == Event::Focus(true) {
                        let headless = controller.borrow().clone().expect("set");
                        // Typed text is never merged.
                        for _ in 0..MAX_QUEUED + 10 {
                            headless.input(crate::RawInput::Utf16(u16::from(b'a')));
                        }
                    }
                    Handled::Yes
                },
            )
            .expect("valid options")
        };
        *controller.borrow_mut() = Some(headless.clone());
        headless.input(crate::RawInput::Focus(true));
        assert_eq!(*delivered.borrow(), 1 + MAX_QUEUED);
        assert_eq!(window.dropped_events(), 10);
        assert_eq!(window.deferred_events(), (MAX_QUEUED + 10) as u64);
    }
}
