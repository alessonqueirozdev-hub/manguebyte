//! A Manguebyte engine for the browser, that builds and runs natively too.
//!
//! The [`Engine`] holds numbered parts. Each part plays an instrument, the
//! polyphonic synth or the drum kit, through a small chain: a resonant
//! low-pass filter, a reverb, then gain and pan. Parts are made and
//! configured with control calls, outside the audio callback; sound is
//! driven by events, each one landing on the exact frame it names.
//!
//! # Time and events
//!
//! Time is a frame counter, a `u64` that starts at 0 when the engine is made
//! and moves on with every frame rendered. An event travels as a 32-byte
//! [`EventRecord`], laid out as [`record::offset`] describes, little endian:
//! `frame` u64, `kind` u8, a padding byte, `part` u16, `note` u8, `channel`
//! u8, two padding bytes, `velocity` f32 (0 to 1), `param` u32 (a
//! [`ParamId`]) and `value` f64 in the parameter's own units. The kinds are
//! listed in [`RecordKind`] with their codes: note on 1, note off 2, param 3,
//! choke 4 (the part's notes stop at once) and all notes off 5 (the part
//! releases every note). A record with an unknown kind or a field out of
//! range is ignored, never a panic; [`EventRecord::decode`] gives the rules.
//!
//! When a quantum of frames is rendered, an event whose frame is inside it
//! lands on that frame; an event whose frame has already passed lands on the
//! first frame of the quantum; an event further ahead waits. Because every
//! part works one frame at a time, the output does not depend on the size
//! of the quanta: one frame at a time or 512 at once gives the same samples,
//! bit for bit.
//!
//! # Parts and parameters
//!
//! A part is a `u16` the caller chooses, made with
//! [`Engine::create_part`] as an [`InstrumentKind::Synth`] or
//! [`InstrumentKind::Drums`] and given its stages with
//! [`Engine::set_chain`]. Its parameters have stable text identifiers,
//! listed in [`ids`] (`"lowpass.cutoff"`, `"reverb.mix"`, `"gain"`, `"pan"`,
//! `"synth.cutoff"`, `"kick.level"` and so on); an event names one by its
//! [`ParamId`], the FNV-1a hash of the identifier. [`Engine::params_json`]
//! describes a part's parameters as JSON: range, default, unit, the curve a
//! knob follows, smoothing, in the format of
//! [`ParamSet::descriptions_json`](manguebyte_params::ParamSet::descriptions_json).
//!
//! The drum kit follows General MIDI: kick 36, snare 38, closed hat 42,
//! open hat 46, crash 49, ride 51, which Aratu writes `b`, `s`, `c`, `o`,
//! `x` and `r` ([`DRUM_MAP`], [`drum_map_json`]). Striking the closed hat
//! chokes the open hat.
//!
//! # Between threads
//!
//! [`event_ring`] is a single-producer single-consumer ring of records with
//! a fixed byte layout (see [`ring`]), so the same ring can live in a
//! `SharedArrayBuffer` shared by a page's main thread and its audio
//! worklet; [`Engine::drain`] moves its records into the engine before each
//! quantum. When shared memory is not available, records can be copied in
//! a message and handed over with [`Engine::push_bytes`].
//!
//! A page gets `SharedArrayBuffer` only when it is cross-origin isolated:
//! it must be served with the headers `Cross-Origin-Opener-Policy:
//! same-origin` and `Cross-Origin-Embedder-Policy: require-corp`.
//! Without them the JavaScript helper falls back to messages.
//!
//! # In a browser
//!
//! Built for `wasm32-unknown-unknown`, the crate is a WebAssembly module
//! with the exports in [`ffi`] and no imports. Next to the crate,
//! `js/worklet.js` is an `AudioWorkletProcessor` that instantiates the
//! module and renders through it, `js/engine.js` is the main-thread helper
//! that creates the node, owns the ring and maps `AudioContext` time to
//! frames, and `js/ring.js` is the ring both use. `examples/web/` is a page
//! that plays a drum pattern and a bass line.
//!
//! # Real time
//!
//! [`Engine::push`], [`Engine::push_bytes`], [`Engine::drain`] and
//! [`Engine::render`] never allocate, lock or panic, and rendering holds
//! [`NoDenormals`](manguebyte_core::NoDenormals) (which does nothing in
//! WebAssembly, where the reverb is put to sleep in silence instead).
//! `tests/realtime.rs` checks the first part of that with a counting
//! allocator.

#![deny(unsafe_code)]

mod chain;
mod drums;
mod engine;
#[allow(unsafe_code)]
pub mod ffi;
mod params;
mod queue;
pub mod record;
pub mod ring;
mod synth;

pub use drums::{DAMP_SECONDS, DRUM_MAP, Drum, DrumNote, HAT_CHOKE_SECONDS, drum_map_json};
pub use engine::{
    Chain, Engine, EngineConfig, EngineError, EngineStats, InstrumentKind, Pushed,
};
pub use manguebyte_params::ParamId;
pub use params::ids;
pub use record::{Event, EventRecord, RECORD_SIZE, RecordKind};
pub use ring::{RingConsumer, RingProducer, event_ring, event_ring_starting_at, ring_bytes};
pub use synth::SYNTH_VOICES;
