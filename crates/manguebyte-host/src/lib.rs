//! Plugin hosting for the Manguebyte audio framework.
//!
//! This crate loads plugins made by anyone and runs them as
//! [`Processor`](manguebyte_dsp::Processor)s, so a hosted plugin goes
//! wherever one of the framework's own processors goes: a device callback,
//! a chain, a graph, an offline render.
//!
//! Each plugin format is a module behind a feature of the same name, and
//! one module speaks for both:
//!
//! - [`clap`]: CLAP plugins.
//! - [`vst3`]: VST 3 plugins.
//! - [`lv2`]: LV2 plugins, read from their bundles' Turtle files.
//! - [`au`]: Audio Units (version 2), found and instantiated through
//!   AudioToolbox on macOS, or hosted in process from a factory function
//!   on any system.
//! - [`any`]: any of them through the same types, for an application
//!   that lists, loads, restores and runs plugins without caring which
//!   format each one is.
//! - [`scan`]: finding the plugins installed on a system and listing them
//!   in a process of their own, so that one that crashes or hangs cannot
//!   take the host down, with a cache of what was found.
//!
//! Whatever the format, a hosted plugin comes in two halves. The half that
//! loads, activates, lists parameters and saves state stays on the thread
//! that created it, and the compiler keeps it there. Activating it yields
//! the half that processes audio, which can be sent to the audio thread.
//! While audio is processed the two talk through wait-free queues and
//! never through a lock.
//!
//! Loading a plugin is `unsafe`: it runs code this crate cannot check, and
//! the caller vouches for it. Everything after that is safe to call.

#[cfg(any(feature = "clap", feature = "vst3", feature = "lv2", feature = "au"))]
pub mod any;
#[cfg(feature = "au")]
pub mod au;
#[cfg(any(feature = "clap", feature = "vst3"))]
mod bundle;
#[cfg(feature = "clap")]
pub mod clap;
#[cfg(any(feature = "lv2", feature = "au"))]
mod delivery;
#[cfg(feature = "lv2")]
pub mod lv2;
#[cfg(any(feature = "clap", feature = "vst3", feature = "lv2", feature = "au"))]
pub mod scan;
#[cfg(any(feature = "clap", feature = "vst3", feature = "lv2", feature = "au"))]
mod threads;
#[cfg(feature = "vst3")]
pub mod vst3;
