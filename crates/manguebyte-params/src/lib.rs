//! Parameters: the values a person, a host or an automation lane can change
//! while audio is running.
//!
//! - [`FloatParam`], [`IntParam`], [`BoolParam`] and [`ChoiceParam`] hold one
//!   value each, and [`Param`] is what they have in common.
//! - [`FloatRange`] shapes how a control feels: linear, skewed or logarithmic.
//! - [`ParamSet`] groups parameters, finds them by identifier, and saves and
//!   restores them as [`ParamState`].
//! - [`ParamSmoother`] gives the audio thread a click-free view of a value.
//! - [`ParamDescription`] is what an editor needs to draw a control for a
//!   parameter, as data: [`Param::describe`] makes one,
//!   [`ParamSet::descriptions_json`] writes a whole set as JSON, and
//!   [`ParamSet::value_to_text`] and [`ParamSet::text_to_value`] convert
//!   values in the parameter's own words.
//!
//! # Threads
//!
//! A parameter stores its value in an atomic and every method takes `&self`,
//! so a set can be shared in an `Arc` between the audio thread, a user
//! interface and a host with no locks and no possibility of a data race.
//! There are deliberately no change listeners: a callback fired from
//! whichever thread happened to write the value is the classic way for
//! user-interface code to end up running on the audio thread. Readers poll
//! instead, with [`ParamSmoother::follow`] or [`Changed`] on the audio thread
//! and a timer on the interface side.
//!
//! Reading and writing values is real-time safe. Converting to and from
//! text, describing parameters, and saving and loading state, allocate and
//! belong elsewhere.

#![forbid(unsafe_code)]

mod describe;
mod param;
mod range;
mod set;
mod smoother;

pub use describe::{Curve, ParamDescription, ParamScale};
pub use manguebyte_core::ParamId;
pub use param::{BoolParam, ChoiceParam, FloatParam, IntParam, Param, ParamFlags, ParamInfo};
pub use range::FloatRange;
pub use set::{LoadReport, ParamSet, ParamSetError, ParamState, ParseStateError};
pub use smoother::{Changed, ParamSmoother, Smoothing};
