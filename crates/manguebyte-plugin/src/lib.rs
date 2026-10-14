//! The plugin API: write a processor once and run it anywhere.
//!
//! A [`Plugin`] is a set of parameters plus a function that processes audio
//! and events. It knows nothing about any plugin format. Format wrappers
//! (CLAP, VST3 and the others), the standalone application wrapper and the
//! test [`Harness`] all drive the same trait, so what is tested here is what
//! runs in a host.
//!
//! ```
//! use std::sync::Arc;
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::{ProcessContext, ProcessSpec};
//! use manguebyte_params::{FloatParam, FloatRange, param_set};
//! use manguebyte_plugin::{AudioLayout, Harness, Plugin, PluginInfo, PluginKind};
//!
//! struct GainParams { gain: FloatParam }
//! param_set!(GainParams { gain });
//!
//! struct Gain { params: Arc<GainParams> }
//!
//! impl Plugin for Gain {
//!     const INFO: PluginInfo = PluginInfo {
//!         id: "org.example.gain",
//!         name: "Gain",
//!         vendor: "Example",
//!         version: "1.0.0",
//!         description: "Turns things up and down",
//!         kind: PluginKind::Effect,
//!     };
//!     const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
//!     type Params = GainParams;
//!
//!     fn new() -> Self {
//!         let range = FloatRange::linear(0.0, 2.0);
//!         Self { params: Arc::new(GainParams { gain: FloatParam::new("gain", "Gain", range, 1.0) }) }
//!     }
//!     fn params(&self) -> Arc<GainParams> { Arc::clone(&self.params) }
//!     fn prepare(&mut self, _: &ProcessSpec) {}
//!     fn process(&mut self, buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
//!         buffer.apply_gain(self.params.gain.value());
//!     }
//! }
//!
//! // No host needed to try it out.
//! let mut harness = Harness::<Gain>::new(48_000.0, 256);
//! harness.params().gain.set(0.5);
//!
//! let mut input = AudioBuffer::<f32>::new(2, 100);
//! input.channel_mut(0).fill(1.0);
//! let output = harness.process(&input);
//! assert_eq!(output.channel(0), &[0.5; 100]);
//! ```
//!
//! A plugin can also offer an editor, through [`Plugin::editor`] and the
//! contract in the [`editor`] module.

#![forbid(unsafe_code)]

pub mod editor;
mod harness;
mod state;

use std::sync::Arc;

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_params::ParamSet;

use editor::EditorSpec;
pub use harness::Harness;
pub use state::{StateError, load_state, save_state};

/// What sort of plugin this is, so a host can list it in the right place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginKind {
    /// Makes sound from notes.
    Instrument,
    /// Changes sound.
    Effect,
    /// Changes or generates notes without touching sound.
    NoteEffect,
    /// Measures sound without changing it.
    Analyzer,
}

/// The fixed facts about a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginInfo {
    /// A globally unique identifier in reverse-domain form, such as
    /// `"com.example.reverb"`. Hosts store it in sessions, so it must never
    /// change once a version has shipped.
    pub id: &'static str,
    /// The name shown to people.
    pub name: &'static str,
    /// Who makes it.
    pub vendor: &'static str,
    /// Its version, such as `"1.2.0"`.
    pub version: &'static str,
    /// One line about what it does.
    pub description: &'static str,
    /// What sort of plugin it is.
    pub kind: PluginKind,
}

/// How many channels a plugin reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioLayout {
    /// Channels of the main input. Zero for an instrument.
    pub inputs: usize,
    /// Channels of the main output.
    pub outputs: usize,
    /// Channels of the sidechain input. Zero if there is none.
    pub sidechain: usize,
}

impl AudioLayout {
    /// Stereo in, stereo out.
    pub const STEREO_EFFECT: Self = Self {
        inputs: 2,
        outputs: 2,
        sidechain: 0,
    };
    /// Mono in, mono out.
    pub const MONO_EFFECT: Self = Self {
        inputs: 1,
        outputs: 1,
        sidechain: 0,
    };
    /// No input, stereo out.
    pub const STEREO_INSTRUMENT: Self = Self {
        inputs: 0,
        outputs: 2,
        sidechain: 0,
    };
}

/// An audio plugin, independent of any plugin format.
///
/// # Threads
///
/// The plugin value itself lives on the audio thread: [`process`] and
/// [`reset`] are called there and must not allocate, lock, block or panic.
/// [`prepare`] is called while audio is stopped and may take its time.
///
/// Everything another thread needs goes through the parameters, which are
/// shared in an [`Arc`] and are safe to read and write from anywhere. A host
/// saving or restoring state, a user interface moving a control and the
/// audio thread reading a value never need to coordinate. An editor gets
/// what it shares with the plugin once, from [`editor`], and never sees the
/// plugin itself.
///
/// [`process`]: Self::process
/// [`reset`]: Self::reset
/// [`prepare`]: Self::prepare
/// [`editor`]: Self::editor
pub trait Plugin: Send + 'static {
    /// The fixed facts about the plugin.
    const INFO: PluginInfo;
    /// How many channels it reads and writes.
    const LAYOUT: AudioLayout;
    /// Its parameters.
    type Params: ParamSet + 'static;

    /// Creates the plugin, with its parameters at their defaults.
    fn new() -> Self
    where
        Self: Sized;

    /// The plugin's parameters, shared with whoever else needs them.
    fn params(&self) -> Arc<Self::Params>;

    /// Gets ready to process under `spec`. Called before the first block and
    /// again whenever the sample rate or block size changes.
    fn prepare(&mut self, spec: &ProcessSpec);

    /// Processes one block in place.
    ///
    /// `buffer` has [`LAYOUT.outputs`](AudioLayout::outputs) channels. The
    /// first [`LAYOUT.inputs`](AudioLayout::inputs) of them hold the input;
    /// any others are silent. `context` carries the events of the block with
    /// the frame each falls on, the transport, and the sidechain if there is
    /// one.
    ///
    /// A plugin can learn about parameter changes in either of two ways,
    /// and both are always correct:
    ///
    /// - **Poll.** The parameters already hold the value they will have at
    ///   the end of the block. Reading them once per block is all a simple
    ///   plugin needs.
    /// - **Follow the events.** Every change is also in `context.events`
    ///   with its exact frame, whoever made it: host automation, a user
    ///   interface writing to the parameter, or restored state. A plugin
    ///   that tracks a value from these events alone never misses a change
    ///   and never needs to poll.
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>);

    /// Clears internal state such as filter memory and tails, without
    /// touching parameters.
    fn reset(&mut self) {}

    /// How many frames late the output is compared with the input.
    fn latency_samples(&self) -> usize {
        0
    }

    /// How to open the plugin's editor, if it has one. `None` unless
    /// overridden, so a plugin without an editor writes nothing.
    ///
    /// Whatever runs the plugin calls this once, on the thread that creates
    /// the plugin, right after [`params`](Self::params) and before any audio
    /// runs, as the [`Harness`] does. It keeps what it gets for the life of
    /// the plugin instance and opens editors from it on the host's main
    /// thread, while the plugin itself runs on the audio thread. That is
    /// why the spec is `Send + Sync + 'static`: it can hold `Arc`s of what
    /// it shares with the plugin, such as the parameters and meter
    /// readings, and the compiler refuses anything that points at the
    /// plugin. See the [`editor`] module for an example.
    fn editor(&self) -> Option<Box<dyn EditorSpec>> {
        None
    }
}

/// Notices parameter changes that did not come from the host, and turns
/// them into events.
///
/// A parameter can be written at any moment by a user interface or by a
/// state restore. Those writes go straight to the parameter and would
/// otherwise be visible only to a plugin that polls. A wrapper keeps one of
/// these, and before each block asks it to add a [`EventKind::ParamValue`]
/// event at frame 0 for every parameter whose value is not what the plugin
/// last saw. The event stream is then a complete record of every change.
///
/// All methods after [`new`](Self::new) are real-time safe.
#[derive(Debug)]
pub struct ParamWatcher {
    /// The value of each parameter as the plugin last saw it, in the order
    /// the set visits them.
    seen: Vec<f64>,
}

impl ParamWatcher {
    /// Starts watching `params` from their current values. Allocates.
    pub fn new(params: &dyn ParamSet) -> Self {
        let mut seen = Vec::with_capacity(params.len());
        params.visit(&mut |param| seen.push(param.plain()));
        Self { seen }
    }

    /// Adds an event at frame 0 for every parameter that has changed since
    /// [`remember`](Self::remember) was last called.
    ///
    /// Call it on an empty buffer, before the host's own events are added,
    /// so that host automation on frame 0 comes later in the stream and
    /// takes precedence. If the buffer fills up, the remaining changes are
    /// picked up on the next block.
    pub fn emit_changes(&mut self, params: &dyn ParamSet, events: &mut EventBuffer) {
        let mut seen = self.seen.iter_mut();
        params.visit(&mut |param| {
            let Some(seen) = seen.next() else {
                return;
            };
            let value = param.plain();
            if value != *seen {
                let kind = EventKind::ParamValue {
                    param: param.param_id(),
                    value,
                };
                if events.push(Event { offset: 0, kind }).is_ok() {
                    *seen = value;
                }
            }
        });
    }

    /// Records the parameters' values as seen. Call it after the block's
    /// events have been stored with [`apply_param_events`].
    pub fn remember(&mut self, params: &dyn ParamSet) {
        let mut seen = self.seen.iter_mut();
        params.visit(&mut |param| {
            if let Some(seen) = seen.next() {
                *seen = param.plain();
            }
        });
    }

    /// Stores the parameter values among `events` into `params`, and
    /// records each as seen. Real-time safe.
    ///
    /// This takes the place of [`apply_param_events`] followed by
    /// [`remember`](Self::remember) where another thread may write the
    /// parameters while a block is being prepared, as an editor does when
    /// it stores its edits straight into them. Pass only the events that
    /// did not come from [`emit_changes`](Self::emit_changes), such as the
    /// host's: the announced values were read from the parameters, so
    /// storing them again could undo an edit made in the meantime, and
    /// `emit_changes` has recorded them already.
    ///
    /// No parameter is read back. What is recorded as seen is what the
    /// parameter holds after storing the event's value, as
    /// [`Param::stored_plain`](manguebyte_params::Param::stored_plain)
    /// works it out, so a value another thread stores at any moment after
    /// `emit_changes`, even right after this stores the host's, is not
    /// taken as seen, and the next block announces it.
    ///
    /// ```
    /// use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
    /// use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
    /// use manguebyte_plugin::ParamWatcher;
    ///
    /// struct Params { level: FloatParam, pan: FloatParam }
    /// param_set!(Params { level, pan });
    /// let params = Params {
    ///     level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 4.0), 1.0),
    ///     pan: FloatParam::new("pan", "Pan", FloatRange::linear(-1.0, 1.0), 0.0),
    /// };
    /// let mut watcher = ParamWatcher::new(&params);
    /// let mut events = EventBuffer::with_capacity(8);
    ///
    /// // An editor sets the level; the block announces it.
    /// params.level.set(2.0);
    /// watcher.emit_changes(&params, &mut events);
    /// let announced = events.len();
    /// // The editor sets it again while the block is being prepared, and the
    /// // host moves the pan on frame 10.
    /// params.level.set(3.0);
    /// let pan = params.pan.param_id();
    /// events.push(Event { offset: 10, kind: EventKind::ParamValue { param: pan, value: 0.5 } }).unwrap();
    ///
    /// watcher.apply_and_remember(&params, &events.as_slice()[announced..]);
    /// assert_eq!(params.pan.value(), 0.5);
    /// // The newer edit is kept, and the next block announces it.
    /// assert_eq!(params.level.value(), 3.0);
    /// events.clear();
    /// watcher.emit_changes(&params, &mut events);
    /// assert_eq!(events.as_slice(), [Event {
    ///     offset: 0,
    ///     kind: EventKind::ParamValue { param: params.level.param_id(), value: 3.0 },
    /// }]);
    /// ```
    pub fn apply_and_remember(&mut self, params: &dyn ParamSet, events: &[Event]) {
        for event in events {
            let EventKind::ParamValue { param, value } = event.kind else {
                continue;
            };
            let mut index = 0;
            let mut found = None;
            params.visit(&mut |candidate| {
                if found.is_none() && candidate.param_id() == param {
                    found = Some((index, candidate));
                }
                index += 1;
            });
            if let Some((index, target)) = found {
                target.set_plain(value);
                // Not read back: another thread may have stored a newer
                // value already. The parameter may round or clamp what it
                // is given; `stored_plain` says what it made of it.
                if let (Some(seen), Some(stored)) =
                    (self.seen.get_mut(index), target.stored_plain(value))
                {
                    *seen = stored;
                }
            }
        }
    }
}

/// Stores every parameter value found in `events` into `params`.
///
/// A wrapper calls this before each block so that anything polling a
/// parameter sees the value automation has given it. When a parameter
/// changes more than once in the block, the last change wins. Real-time
/// safe.
pub fn apply_param_events(params: &dyn ParamSet, events: &EventBuffer) {
    for event in events {
        if let EventKind::ParamValue { param, value } = event.kind {
            if let Some(target) = params.find_param(param) {
                target.set_plain(value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
    use manguebyte_params::{FloatParam, FloatRange, Param, ParamInfo, param_set};

    use super::ParamWatcher;

    type After = Box<dyn FnOnce(&FloatParam) + Send>;

    /// A float parameter that runs `after` once, right after the next value
    /// is stored: the moment another thread could store one of its own.
    struct Hooked {
        param: FloatParam,
        after: Mutex<Option<After>>,
    }

    impl Hooked {
        fn then(&self, after: impl FnOnce(&FloatParam) + Send + 'static) {
            *self.after.lock().unwrap() = Some(Box::new(after));
        }
    }

    impl Param for Hooked {
        fn info(&self) -> &ParamInfo {
            self.param.info()
        }
        fn plain(&self) -> f64 {
            self.param.plain()
        }
        fn set_plain(&self, value: f64) {
            self.param.set_plain(value);
            let after = self.after.lock().unwrap().take();
            if let Some(after) = after {
                after(&self.param);
            }
        }
        fn stored_plain(&self, value: f64) -> Option<f64> {
            self.param.stored_plain(value)
        }
        fn default_plain(&self) -> f64 {
            self.param.default_plain()
        }
        fn normalized_to_plain(&self, normalized: f64) -> f64 {
            self.param.normalized_to_plain(normalized)
        }
        fn plain_to_normalized(&self, plain: f64) -> f64 {
            self.param.plain_to_normalized(plain)
        }
        fn step_count(&self) -> Option<u32> {
            self.param.step_count()
        }
        fn plain_to_text(&self, plain: f64) -> String {
            self.param.plain_to_text(plain)
        }
        fn text_to_plain(&self, text: &str) -> Option<f64> {
            self.param.text_to_plain(text)
        }
    }

    struct Params {
        cutoff: Hooked,
    }
    param_set!(Params { cutoff });

    /// An edit stored right after the host's value, before anything could
    /// read the parameter back, is announced on the next block, and the
    /// host's value, rounded as the parameter stores it, is not.
    #[test]
    fn an_edit_stored_right_after_the_hosts_value_is_announced() {
        let params = Params {
            cutoff: Hooked {
                param: FloatParam::new("cutoff", "Cutoff", FloatRange::linear(0.0, 4.0), 1.0),
                after: Mutex::new(None),
            },
        };
        let cutoff = params.cutoff.param_id();
        let mut watcher = ParamWatcher::new(&params);
        let mut events = EventBuffer::with_capacity(8);

        // The host sets 1.2, which an `f32` cannot hold exactly; the person's
        // edit to 3 lands right after.
        params.cutoff.then(|param| param.set(3.0));
        let kind = EventKind::ParamValue {
            param: cutoff,
            value: 1.2,
        };
        watcher.apply_and_remember(&params, &[Event { offset: 0, kind }]);
        assert_eq!(params.cutoff.param.value(), 3.0);

        watcher.emit_changes(&params, &mut events);
        assert_eq!(
            events.as_slice(),
            [Event {
                offset: 0,
                kind: EventKind::ParamValue {
                    param: cutoff,
                    value: 3.0
                }
            }]
        );

        // With no edit in between, the rounded host value is not announced.
        let kind = EventKind::ParamValue {
            param: cutoff,
            value: 1.2,
        };
        watcher.apply_and_remember(&params, &[Event { offset: 0, kind }]);
        events.clear();
        watcher.emit_changes(&params, &mut events);
        assert!(events.is_empty(), "announced {:?}", events.as_slice());
    }
}
