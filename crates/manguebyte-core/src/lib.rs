//! Core building blocks of the Manguebyte audio framework.
//!
//! This crate has no dependencies and holds the types every other Manguebyte
//! crate is built on:
//!
//! - [`Sample`]: the floating-point sample abstraction (`f32` and `f64`).
//! - [`AudioBuffer`]: a planar, multi-channel block of audio.
//! - [`Smoothed`]: click-free parameter ramps.
//! - [`AtomicF32`] / [`AtomicF64`]: lock-free values shared with the audio thread.
//! - [`spsc`]: a wait-free single-producer single-consumer queue.
//! - [`log`]: a log that any thread, the audio thread included, can write to.
//! - [`decibels`]: gain conversions.
//! - [`governor`]: measures each block against its deadline and gives an
//!   instrument a budget to spend, so that it renders less rather than
//!   glitch.
//! - [`NoDenormals`]: stops near-silence from slowing the processor down.
//! - [`ParamId`]: a compact, stable number standing for a parameter.
//! - [`pool`]: rendering independent jobs (voices, divisions, chains) on
//!   several cores inside one audio callback, with output identical to one
//!   thread's.
//! - [`module`]: the file of the binary this code is linked into, such as a
//!   plugin's own library.
//! - [`source`]: sources of audio that are pulled a block at a time, and
//!   audio in memory as one.
//! - [`vector`]: operations on slices of samples (gain, mixing, ramps,
//!   peaks, sums, dot products, interleaving, integer conversions) that use
//!   the processor's vector instructions, chosen at run time.

mod atomic;
mod buffer;
pub mod decibels;
mod denormal;
pub mod governor;
pub mod log;
pub mod module;
mod param_id;
pub mod pool;
mod sample;
mod smoothing;
pub mod source;
pub mod spsc;
pub mod vector;

pub use atomic::{AtomicF32, AtomicF64};
pub use buffer::AudioBuffer;
pub use denormal::NoDenormals;
pub use log::{DrainedLog, LogLevel, LogQueue, LogRecord};
pub use param_id::ParamId;
pub use sample::Sample;
pub use smoothing::Smoothed;
