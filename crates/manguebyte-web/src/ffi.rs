//! The WebAssembly exports: plain `extern "C"` functions over the module's
//! memory, with no binding generator.
//!
//! Built with `cargo build -p manguebyte-web --target wasm32-unknown-unknown`,
//! the crate is a module that imports nothing and exports these functions
//! and its `memory`. Pointers are byte offsets into that memory; `u32` and
//! `i32` are JavaScript numbers, `f64` too.
//!
//! The usual sequence, which `js/worklet.js` follows:
//!
//! 1. [`mb_engine_new`] once, keeping the returned engine.
//! 2. [`mb_create_part`] for each part, and [`mb_set_chain`] as needed.
//! 3. For every quantum: copy records into the staging area at
//!    [`mb_staging_ptr`] and call [`mb_push_staged`]; call [`mb_render`];
//!    read the frames from [`mb_output_ptr`] for channels 0 and 1.
//!
//! The staging area and the output buffers never move, but any call that
//! allocates (making parts, describing parameters) may grow the memory, and
//! a grown memory has a new `ArrayBuffer`: views made on the old one must
//! be made again. [`mb_render`] and [`mb_push_staged`] never allocate.
//!
//! Every function that takes an engine accepts a null pointer and then
//! does nothing, returning 0, a null pointer, [`status::NO_ENGINE`] or NaN.

use crate::drums::drum_map_json;
use crate::engine::{Chain, Engine, EngineConfig, EngineError, InstrumentKind};
use crate::record::RECORD_SIZE;
use manguebyte_params::ParamId;

/// The version of this interface, from [`mb_abi_version`]. It changes
/// whenever an export changes meaning.
pub const ABI_VERSION: u32 = 1;

/// The most records the staging area can hold.
pub const MAX_STAGING_RECORDS: u32 = 65_536;

/// What the control exports return.
pub mod status {
    /// Done.
    pub const OK: i32 = 0;
    /// The engine pointer was null.
    pub const NO_ENGINE: i32 = -1;
    /// The part number is above 65 535.
    pub const BAD_PART: i32 = -2;
    /// The instrument code is not one of
    /// [`InstrumentKind::code`](crate::InstrumentKind::code).
    pub const BAD_INSTRUMENT: i32 = -3;
    /// The chain has a bit that is not a stage.
    pub const BAD_CHAIN: i32 = -4;
    /// Every part slot is taken.
    pub const TOO_MANY_PARTS: i32 = -5;
    /// There is no such part.
    pub const NO_SUCH_PART: i32 = -6;
    /// A configuration value was out of range.
    pub const INVALID_CONFIG: i32 = -7;
}

/// An engine with the memory the exports hand to JavaScript.
pub struct WebEngine {
    engine: Engine,
    staging: Vec<u8>,
    text: String,
}

impl WebEngine {
    /// The engine inside.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The engine inside, mutably.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

/// # Safety
///
/// `engine` is null or a pointer from [`mb_engine_new`] that has not been
/// freed, and nothing else uses it for the lifetime `'a`.
unsafe fn web<'a>(engine: *mut WebEngine) -> Option<&'a mut WebEngine> {
    // SAFETY: the caller promises the pointer is null or live and unshared.
    unsafe { engine.as_mut() }
}

fn status_of(result: Result<(), EngineError>) -> i32 {
    match result {
        Ok(()) => status::OK,
        Err(EngineError::TooManyParts { .. }) => status::TOO_MANY_PARTS,
        Err(EngineError::NoSuchPart(_)) => status::NO_SUCH_PART,
        Err(EngineError::InvalidConfig(_)) => status::INVALID_CONFIG,
    }
}

/// The version of this interface: [`ABI_VERSION`].
#[unsafe(no_mangle)]
pub extern "C" fn mb_abi_version() -> u32 {
    ABI_VERSION
}

/// The size of one event record: 32.
#[unsafe(no_mangle)]
pub extern "C" fn mb_record_size() -> u32 {
    RECORD_SIZE as u32
}

/// Makes an engine, or returns null if an argument is out of range (see
/// [`EngineConfig`]; `staging_records` is 1 to [`MAX_STAGING_RECORDS`]).
/// Free it with [`mb_engine_free`].
#[unsafe(no_mangle)]
pub extern "C" fn mb_engine_new(
    sample_rate: f64,
    max_quantum: u32,
    max_parts: u32,
    pending_capacity: u32,
    staging_records: u32,
) -> *mut WebEngine {
    if !(1..=MAX_STAGING_RECORDS).contains(&staging_records) {
        return core::ptr::null_mut();
    }
    let config = EngineConfig {
        sample_rate,
        max_quantum: max_quantum as usize,
        max_parts: max_parts as usize,
        pending_capacity: pending_capacity as usize,
    };
    match Engine::new(config) {
        Ok(engine) => Box::into_raw(Box::new(WebEngine {
            engine,
            staging: vec![0; staging_records as usize * RECORD_SIZE],
            text: String::new(),
        })),
        Err(_) => core::ptr::null_mut(),
    }
}

/// Frees an engine.
///
/// # Safety
///
/// `engine` is null or a pointer from [`mb_engine_new`] that has not been
/// freed yet; it is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_engine_free(engine: *mut WebEngine) {
    if !engine.is_null() {
        // SAFETY: the pointer came from `Box::into_raw` in `mb_engine_new`
        // and the caller promises it is freed only once.
        drop(unsafe { Box::from_raw(engine) });
    }
}

/// Where the staging area starts: room for [`mb_staging_capacity`]
/// records, written by JavaScript and read by [`mb_push_staged`].
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_staging_ptr(engine: *mut WebEngine) -> *mut u8 {
    // SAFETY: as the caller promises.
    match unsafe { web(engine) } {
        Some(web) => web.staging.as_mut_ptr(),
        None => core::ptr::null_mut(),
    }
}

/// How many records the staging area holds.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_staging_capacity(engine: *mut WebEngine) -> u32 {
    // SAFETY: as the caller promises.
    unsafe { web(engine) }.map_or(0, |web| (web.staging.len() / RECORD_SIZE) as u32)
}

/// Pushes the first `count` records of the staging area, at most its
/// capacity, and returns how many were queued (see
/// [`Engine::push`](crate::Engine::push)). Never allocates.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_push_staged(engine: *mut WebEngine, count: u32) -> u32 {
    // SAFETY: as the caller promises.
    let Some(web) = (unsafe { web(engine) }) else {
        return 0;
    };
    let bytes = (count as usize)
        .saturating_mul(RECORD_SIZE)
        .min(web.staging.len());
    web.engine.push_bytes(&web.staging[..bytes]) as u32
}

/// How many more events can wait in the engine.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_pending_room(engine: *mut WebEngine) -> u32 {
    // SAFETY: as the caller promises.
    unsafe { web(engine) }.map_or(0, |web| web.engine.pending_room() as u32)
}

/// Forgets every waiting event.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_clear_pending(engine: *mut WebEngine) {
    // SAFETY: as the caller promises.
    if let Some(web) = unsafe { web(engine) } {
        web.engine.clear_pending();
    }
}

/// Renders `frames` frames, at most the engine's maximum quantum, and
/// returns how many were rendered. Never allocates.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_render(engine: *mut WebEngine, frames: u32) -> u32 {
    // SAFETY: as the caller promises.
    unsafe { web(engine) }.map_or(0, |web| web.engine.render(frames as usize) as u32)
}

/// Where output channel `channel` (0 left, 1 right) starts, as `f32`s;
/// null for any other channel. Holds the frames of the last
/// [`mb_render`] from its start.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_output_ptr(engine: *mut WebEngine, channel: u32) -> *const f32 {
    // SAFETY: as the caller promises.
    match unsafe { web(engine) } {
        Some(web) if channel < 2 => web.engine.output_ptr(channel as usize),
        _ => core::ptr::null(),
    }
}

/// The frame the next [`mb_render`] starts on. Exact up to 2⁵³.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_frame(engine: *mut WebEngine) -> f64 {
    // SAFETY: as the caller promises.
    unsafe { web(engine) }.map_or(f64::NAN, |web| web.engine.frame() as f64)
}

fn part_number(part: u32) -> Result<u16, i32> {
    u16::try_from(part).map_err(|_| status::BAD_PART)
}

/// Makes part `part` play instrument `kind`
/// ([`InstrumentKind::code`](crate::InstrumentKind::code)) through the
/// stages in `chain` ([`Chain::bits`](crate::Chain::bits)), replacing any
/// part already there. Returns a [`status`]. Allocates.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_create_part(
    engine: *mut WebEngine,
    part: u32,
    kind: u32,
    chain: u32,
) -> i32 {
    // SAFETY: as the caller promises.
    let Some(web) = (unsafe { web(engine) }) else {
        return status::NO_ENGINE;
    };
    let part = match part_number(part) {
        Ok(part) => part,
        Err(status) => return status,
    };
    let Some(kind) = InstrumentKind::from_code(kind) else {
        return status::BAD_INSTRUMENT;
    };
    let Some(chain) = Chain::from_bits(chain) else {
        return status::BAD_CHAIN;
    };
    status_of(web.engine.create_part_with(part, kind, chain))
}

/// Removes a part. Returns a [`status`].
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_remove_part(engine: *mut WebEngine, part: u32) -> i32 {
    // SAFETY: as the caller promises.
    let Some(web) = (unsafe { web(engine) }) else {
        return status::NO_ENGINE;
    };
    match part_number(part) {
        Ok(part) => status_of(web.engine.remove_part(part)),
        Err(status) => status,
    }
}

/// Sets which stages a part's chain has
/// ([`Chain::bits`](crate::Chain::bits)). Returns a [`status`].
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_set_chain(engine: *mut WebEngine, part: u32, chain: u32) -> i32 {
    // SAFETY: as the caller promises.
    let Some(web) = (unsafe { web(engine) }) else {
        return status::NO_ENGINE;
    };
    let part = match part_number(part) {
        Ok(part) => part,
        Err(status) => return status,
    };
    let Some(chain) = Chain::from_bits(chain) else {
        return status::BAD_CHAIN;
    };
    status_of(web.engine.set_chain(part, chain))
}

/// Writes the parameters of a part as JSON
/// ([`Engine::params_json`](crate::Engine::params_json)) into the engine's
/// text buffer and returns where it starts, or null if there is no such
/// part. [`mb_text_len`] gives its length in bytes, UTF-8. The text stays
/// until the next call that writes text. Allocates.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_params_json(engine: *mut WebEngine, part: u32) -> *const u8 {
    // SAFETY: as the caller promises.
    let Some(web) = (unsafe { web(engine) }) else {
        return core::ptr::null();
    };
    let json = part_number(part)
        .ok()
        .and_then(|part| web.engine.params_json(part));
    match json {
        Some(json) => {
            web.text = json;
            web.text.as_ptr()
        }
        None => {
            web.text.clear();
            core::ptr::null()
        }
    }
}

/// Writes the drum map ([`drum_map_json`](crate::drum_map_json)) into the
/// engine's text buffer and returns where it starts; see
/// [`mb_params_json`]. Allocates.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_drum_map_json(engine: *mut WebEngine) -> *const u8 {
    // SAFETY: as the caller promises.
    let Some(web) = (unsafe { web(engine) }) else {
        return core::ptr::null();
    };
    web.text = drum_map_json();
    web.text.as_ptr()
}

/// The length in bytes of the text the last text call wrote.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_text_len(engine: *mut WebEngine) -> u32 {
    // SAFETY: as the caller promises.
    unsafe { web(engine) }.map_or(0, |web| web.text.len() as u32)
}

/// The value a parameter of a part holds, or NaN if there is no such part
/// or parameter. `param` is a [`ParamId`].
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_param_value(engine: *mut WebEngine, part: u32, param: u32) -> f64 {
    // SAFETY: as the caller promises.
    let Some(web) = (unsafe { web(engine) }) else {
        return f64::NAN;
    };
    part_number(part)
        .ok()
        .and_then(|part| web.engine.param_value_by_id(part, ParamId(param)))
        .unwrap_or(f64::NAN)
}

/// How many voices of a part are sounding, or -1 if there is no such part.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_active_voices(engine: *mut WebEngine, part: u32) -> i32 {
    // SAFETY: as the caller promises.
    let Some(web) = (unsafe { web(engine) }) else {
        return status::NO_ENGINE;
    };
    part_number(part)
        .ok()
        .and_then(|part| web.engine.active_voices(part))
        .map_or(-1, |voices| voices as i32)
}

/// One of the engine's counters
/// ([`EngineStats`](crate::EngineStats)): 0 queued, 1 ignored, 2 dropped,
/// 3 late, 4 applied, 5 unrouted. NaN for any other index.
///
/// # Safety
///
/// `engine` is null or a live pointer from [`mb_engine_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mb_stat(engine: *mut WebEngine, index: u32) -> f64 {
    // SAFETY: as the caller promises.
    let Some(web) = (unsafe { web(engine) }) else {
        return f64::NAN;
    };
    let stats = web.engine.stats();
    let value = match index {
        0 => stats.queued,
        1 => stats.ignored,
        2 => stats.dropped,
        3 => stats.late,
        4 => stats.applied,
        5 => stats.unrouted,
        _ => return f64::NAN,
    };
    value as f64
}
