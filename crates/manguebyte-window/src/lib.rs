//! Platform windows for the Manguebyte user interface.
//!
//! A [`Window`] is a child of the window a host gives a plugin editor (a
//! [`ParentWindow`] from the editor contract). It belongs to the host's
//! main thread, sends everything that happens to it to a
//! [`WindowHandler`] as [`Event`]s, and shows premultiplied RGBA
//! [`Pixmap`]s through a back buffer of its own: presenting converts and
//! copies only the rectangles asked for, and an expose is answered from
//! the back buffer without the handler.
//!
//! Two backends implement it:
//!
//! - **Win32** ([`Window::open`] with a `Win32` parent, on Windows): a
//!   `WS_CHILD` window, presenting with `SetDIBitsToDevice`. See
//!   [`win32`] for what it does and does not do to the process.
//! - **Headless** ([`Window::headless`], on every system): no system window
//!   at all. A test drives it through [`Headless`], which injects
//!   [`RawInput`], exposes, resizes, fires timers and destroys the parent,
//!   and reads back what the window shows.
//!
//! Both run the same code for everything but the system calls: damage
//! merging ([`Damage`]), the back buffer and its pixel conversion, the
//! conversion from device to logical pixels, pointer capture and click
//! counting, timers, and the queue that keeps the handler from being
//! re-entered.
//!
//! # Units
//!
//! Sizes, damage and presented pixmaps are in device pixels. Pointer
//! events carry both the device position and the logical one, which is
//! the device position divided by [`scale`](Window::scale). The scale is
//! the host's when it gave one ([`WindowOptions::with_scale`],
//! [`set_host_scale`](Window::set_host_scale)), and otherwise the system's
//! for the window.
//!
//! # Reentrancy
//!
//! A handler may resize the window, take the keyboard, or do anything else
//! that makes the system send the window a message at once. Such a message
//! arrives while the handler is still running; the window queues the event
//! and delivers it right after the handler returns, in order. The handler
//! is never re-entered. While it runs, repeated timer ticks, pointer moves
//! and paints are merged, and at most [`MAX_QUEUED`] events wait; what is
//! dropped beyond that is counted ([`Window::dropped_events`]).
//!
//! ```
//! use std::cell::RefCell;
//! use std::rc::Rc;
//! use manguebyte_window::{Event, Handled, PhysicalSize, RawInput, TimerId, Window, WindowCx, WindowOptions};
//!
//! let seen = Rc::new(RefCell::new(Vec::new()));
//! let log = Rc::clone(&seen);
//! let (window, headless) = Window::headless(
//!     WindowOptions::new(PhysicalSize::new(200, 100)),
//!     move |window: &WindowCx<'_>, event| {
//!         log.borrow_mut().push(format!("{event:?}"));
//!         if event == Event::Timer(TimerId(1)) {
//!             // The system answers at once; the event waits for us.
//!             window.resize(PhysicalSize::new(300, 100)).expect("open");
//!             log.borrow_mut().push("returning".to_string());
//!         }
//!         Handled::Yes
//!     },
//! )
//! .expect("valid options");
//!
//! window.set_timer(TimerId(1), 16).expect("open");
//! headless.input(RawInput::Timer(TimerId(1)));
//! assert_eq!(
//!     *seen.borrow(),
//!     [
//!         "Timer(TimerId(1))",
//!         "returning",
//!         "Resized(PhysicalSize { width: 300, height: 100 })",
//!     ]
//! );
//! assert_eq!(window.deferred_events(), 1);
//! ```
//!
//! # Hygiene
//!
//! Nothing here keeps mutable statics, starts threads, installs hooks or
//! changes a process-wide setting, and no name it gives the system is the
//! same in two binaries (ADR 0007). This crate never reaches a crate that
//! processes audio.

#![deny(unsafe_code)]

mod convert;
mod damage;
mod dispatch;
mod event;
mod headless;
mod pixels;
mod pointer;
mod window;

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod win32;

pub use damage::{Damage, MAX_RECTS};
pub use dispatch::{MAX_QUEUED, WindowHandler};
pub use event::{
    Button, Event, Handled, Key, KeyEvent, Modifiers, PhysicalPoint, PhysicalSize, PointerEvent,
    RawInput, TimerId, WHEEL_DELTA,
};
pub use headless::Headless;
pub use manguebyte_gfx::{IRect, Pixmap};
pub use manguebyte_plugin::editor::{ParentWindow, WindowApi};
pub use window::{Window, WindowCx, WindowError, WindowOptions};
