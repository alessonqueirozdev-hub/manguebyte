//! The glue between the LV2 C interface and a [`Plugin`].
//!
//! # Threads
//!
//! LV2 sorts a plugin's functions into threading classes, and this file
//! leans on those rules instead of on locks:
//!
//! - `instantiate`, `activate`, `deactivate`, `cleanup` and the state
//!   extension's `restore` are never called at the same time as any other
//!   function of the same instance.
//! - `connect_port` and `run` are called on the audio thread, never at the
//!   same time as each other.
//! - The state extension's `save` may be called on another thread while
//!   `run` is.
//!
//! So the audio-side state ([`Audio`]) is only ever touched by one thread at
//! a time and lives in an `UnsafeCell`. `save` and `restore` only touch the
//! parameters, which are atomics, and the fields of [`Instance`] that never
//! change after `instantiate`.
//!
//! # One event stream
//!
//! Each block the plugin is given one stream of events, as the CLAP and
//! VST 3 wrappers give it: first, on frame 0, every parameter that changed
//! without the host (a restored state, for one); then, on frame 0, the
//! parameters whose control ports the host moved; then the MIDI of the
//! control sequence, each message on its frame. Where the host places a
//! `time:Position` inside the block, the block is processed in two pieces
//! so the plugin sees the new transport from that frame on, and a block
//! longer than the bound the host promised is processed in pieces of at
//! most that bound. Each piece gets the events that fall in it, counted
//! from its own start. A `time:Position` stamped past the end of the block
//! takes effect from the start of the next block, moved back by the frames
//! it was late.

use std::cell::UnsafeCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::Arc;

use manguebyte_core::{AudioBuffer, NoDenormals};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Transport};
use manguebyte_midi::MidiMessage;
use manguebyte_params::ParamSet;
use manguebyte_plugin::{ParamWatcher, Plugin, load_state, save_state};

use crate::atom::{Events, NumberTypes, Object, Sequence, number};
use crate::ports::{PortLayout, PortRole};
use crate::sys::*;
use crate::uri::STATE_KEY;

/// Most events delivered to the plugin in one block. Extra events are
/// dropped rather than allocated for.
pub const EVENTS_PER_BLOCK: usize = 4_096;

/// The longest piece a block is processed in when the host does not give
/// `buf-size:maxBlockLength`.
pub const DEFAULT_MAX_BLOCK: usize = 4_096;

/// The longest piece a block is ever processed in, whatever the host says,
/// so a host's bound cannot make the plugin allocate without limit.
pub const LARGEST_MAX_BLOCK: usize = 65_536;

/// Largest state a host may hand back.
const MAX_STATE_BYTES: usize = 64 << 20;

/// Most entries read from the host's feature and option lists, in case one
/// is not terminated.
const MAX_LIST_ENTRIES: usize = 4_096;

/// Runs `work`, turning a panic into `fallback`.
///
/// A panic must never cross into the host: unwinding through C is not
/// defined, and aborting would take the user's session with it.
fn guard<R>(fallback: R, work: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(work)).unwrap_or(fallback)
}

// --- Descriptor -------------------------------------------------------------

/// The `LV2_Descriptor` of one plugin type, made by
/// [`descriptor!`](crate::descriptor) and kept in an immutable static.
#[repr(transparent)]
pub struct Descriptor(LV2_Descriptor);

// SAFETY: the descriptor is never changed after it is built. Its only
// pointer is to the plugin's URI, a C string in an immutable constant, and
// its functions may be called from any thread the LV2 rules allow.
unsafe impl Sync for Descriptor {}

impl std::fmt::Debug for Descriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Descriptor").field(&self.uri()).finish()
    }
}

impl Descriptor {
    /// The descriptor of `P`, whose URI is `uri`. Use
    /// [`descriptor!`](crate::descriptor), which works the URI out from
    /// the plugin's id.
    #[doc(hidden)]
    pub const fn new<P: Plugin>(uri: &'static CStr) -> Self {
        Self(LV2_Descriptor {
            URI: uri.as_ptr(),
            instantiate: Some(instantiate::<P>),
            connect_port: Some(connect_port::<P>),
            activate: Some(activate::<P>),
            run: Some(run::<P>),
            deactivate: Some(deactivate::<P>),
            cleanup: Some(cleanup::<P>),
            extension_data: Some(extension_data::<P>),
        })
    }

    /// The C structure, for a host.
    pub const fn as_ptr(&self) -> *const LV2_Descriptor {
        &self.0
    }

    /// The C structure's functions, for a host written in Rust.
    pub const fn raw(&self) -> &LV2_Descriptor {
        &self.0
    }

    /// The plugin's URI.
    pub fn uri(&self) -> &CStr {
        // SAFETY: `new` set it from a `&'static CStr`.
        unsafe { CStr::from_ptr(self.0.URI) }
    }
}

// --- What the host offers ---------------------------------------------------

/// The features of the host that this wrapper uses.
struct Features {
    map: Option<LV2_URID_Map>,
    options: *const LV2_Options_Option,
    log: Option<LV2_Log_Log>,
}

impl Features {
    /// Reads the host's null-terminated list of features.
    ///
    /// # Safety
    ///
    /// `list` must be null or a null-terminated array of pointers to valid
    /// features, each with a URI that is null or NUL-terminated.
    unsafe fn read(list: *const *const LV2_Feature) -> Self {
        let mut found = Self {
            map: None,
            options: ptr::null(),
            log: None,
        };
        if list.is_null() {
            return found;
        }
        for index in 0..MAX_LIST_ENTRIES {
            // SAFETY: the list is terminated by a null pointer, which ends
            // the loop before reading past it.
            let feature = unsafe { *list.add(index) };
            // SAFETY: a non-null entry points to a valid feature.
            let Some(feature) = (unsafe { feature.as_ref() }) else {
                break;
            };
            if feature.URI.is_null() || feature.data.is_null() {
                continue;
            }
            // SAFETY: a feature's URI is a NUL-terminated string.
            let uri = unsafe { CStr::from_ptr(feature.URI) };
            if uri == LV2_URID__map {
                // SAFETY: this feature's data is an `LV2_URID_Map`.
                found.map = Some(unsafe { feature.data.cast::<LV2_URID_Map>().read() });
            } else if uri == LV2_OPTIONS__options {
                found.options = feature.data.cast();
            } else if uri == LV2_LOG__log {
                // SAFETY: this feature's data is an `LV2_Log_Log`.
                found.log = Some(unsafe { feature.data.cast::<LV2_Log_Log>().read() });
            }
        }
        found
    }
}

/// Maps a URI with the host's map. `None` if it has no function or
/// returns zero.
///
/// # Safety
///
/// `map` must be the data of the host's `urid:map` feature.
unsafe fn urid(map: &LV2_URID_Map, uri: &CStr) -> Option<LV2_URID> {
    let function = map.map?;
    // SAFETY: the host's own function with the host's own handle.
    let id = unsafe { function(map.handle, uri.as_ptr()) };
    (id != 0).then_some(id)
}

/// The URIDs the wrapper compares against, mapped once in `instantiate`.
#[derive(Debug, Clone, Copy)]
struct Urids {
    sequence: u32,
    object: u32,
    blank: u32,
    resource: u32,
    numbers: NumberTypes,
    string: u32,
    frame_time: u32,
    midi_event: u32,
    position: u32,
    frame: u32,
    speed: u32,
    beats_per_minute: u32,
    beats_per_bar: u32,
    beat_unit: u32,
    bar: u32,
    bar_beat: u32,
    beat: u32,
    max_block_length: u32,
    state_key: u32,
}

impl Urids {
    /// # Safety
    ///
    /// As for [`urid`].
    unsafe fn new(map: &LV2_URID_Map) -> Option<Self> {
        // SAFETY: per this function's contract.
        let get = |uri| unsafe { urid(map, uri) };
        Some(Self {
            sequence: get(LV2_ATOM__Sequence)?,
            object: get(LV2_ATOM__Object)?,
            blank: get(LV2_ATOM__Blank)?,
            resource: get(LV2_ATOM__Resource)?,
            numbers: NumberTypes {
                int: get(LV2_ATOM__Int)?,
                long: get(LV2_ATOM__Long)?,
                float: get(LV2_ATOM__Float)?,
                double: get(LV2_ATOM__Double)?,
            },
            string: get(LV2_ATOM__String)?,
            frame_time: get(LV2_ATOM__frameTime)?,
            midi_event: get(LV2_MIDI__MidiEvent)?,
            position: get(LV2_TIME__Position)?,
            frame: get(LV2_TIME__frame)?,
            speed: get(LV2_TIME__speed)?,
            beats_per_minute: get(LV2_TIME__beatsPerMinute)?,
            beats_per_bar: get(LV2_TIME__beatsPerBar)?,
            beat_unit: get(LV2_TIME__beatUnit)?,
            bar: get(LV2_TIME__bar)?,
            bar_beat: get(LV2_TIME__barBeat)?,
            beat: get(LV2_TIME__beat)?,
            max_block_length: get(LV2_BUF_SIZE__maxBlockLength)?,
            state_key: get(STATE_KEY)?,
        })
    }

    fn is_object(&self, type_: u32) -> bool {
        type_ == self.object || type_ == self.blank || type_ == self.resource
    }
}

/// The value of `buf-size:maxBlockLength` among the host's options.
///
/// # Safety
///
/// `option` must be null or point to a list of options ended by one with a
/// zero key and a null value.
unsafe fn max_block_length(option: *const LV2_Options_Option, urids: &Urids) -> Option<i64> {
    if option.is_null() {
        return None;
    }
    for index in 0..MAX_LIST_ENTRIES {
        // SAFETY: the list is terminated, which ends the loop before
        // reading past it.
        let current = unsafe { option.add(index).read() };
        if current.key == 0 && current.value.is_null() {
            return None;
        }
        if current.context != LV2_OPTIONS_INSTANCE
            || current.key != urids.max_block_length
            || current.value.is_null()
        {
            continue;
        }
        // SAFETY: the value holds `size` bytes of the atom type `type_`.
        unsafe {
            if current.type_ == urids.numbers.int && current.size == 4 {
                return Some(i64::from(current.value.cast::<i32>().read_unaligned()));
            }
            if current.type_ == urids.numbers.long && current.size == 8 {
                return Some(current.value.cast::<i64>().read_unaligned());
            }
        }
    }
    None
}

/// How serious a logged message is.
#[derive(Debug, Clone, Copy)]
enum Level {
    Error,
    Warning,
    Note,
}

/// The host's `log:log`, with the URIDs of the message types.
struct Log {
    handle: *mut c_void,
    printf: unsafe extern "C" fn(*mut c_void, LV2_URID, *const c_char, ...) -> std::ffi::c_int,
    error: u32,
    warning: u32,
    note: u32,
}

impl Log {
    /// # Safety
    ///
    /// `log` must be the data of the host's `log:log` feature, and `map` as
    /// for [`urid`].
    unsafe fn new(log: LV2_Log_Log, map: &LV2_URID_Map) -> Option<Self> {
        // SAFETY: per this function's contract.
        let get = |uri| unsafe { urid(map, uri) };
        Some(Self {
            handle: log.handle,
            printf: log.printf?,
            error: get(LV2_LOG__Error)?,
            warning: get(LV2_LOG__Warning)?,
            note: get(LV2_LOG__Note)?,
        })
    }

    /// Sends one line to the host's log. Allocates: never call it from
    /// `run`.
    fn write(&self, level: Level, message: &str) {
        let kind = match level {
            Level::Error => self.error,
            Level::Warning => self.warning,
            Level::Note => self.note,
        };
        let text: Vec<u8> = message.bytes().filter(|&byte| byte != 0).collect();
        let Ok(text) = CString::new(text) else {
            return;
        };
        // SAFETY: the host's own function and handle, with a format that
        // takes exactly the one C string passed after it.
        unsafe { (self.printf)(self.handle, kind, c"%s\n".as_ptr(), text.as_ptr()) };
    }
}

fn log(log: Option<&Log>, level: Level, message: &str) {
    if let Some(log) = log {
        log.write(level, message);
    }
}

// --- The transport ------------------------------------------------------------

/// Where the host's transport is, as the last `time:Position` said and
/// moved on since by the frames processed.
#[derive(Debug, Clone, Copy, Default)]
struct Position {
    /// Whether the host has sent a position at all.
    known: bool,
    frame: Option<f64>,
    speed: f64,
    beats_per_minute: Option<f64>,
    beats_per_bar: Option<f64>,
    beat_unit: Option<f64>,
    bar: Option<f64>,
    bar_beat: Option<f64>,
    beat: Option<f64>,
}

impl Position {
    /// Takes in the properties of a `time:Position` object.
    ///
    /// The place on the timeline (`time:frame`, `time:bar`, `time:barBeat`
    /// and `time:beat`) is replaced as a whole: an object that gives any of
    /// them forgets the others it does not give, so a place worked out
    /// from an older position never mixes with a new one. Speed, tempo and
    /// time signature keep their values until an object gives new ones.
    ///
    /// Returns whether the object gave a new place.
    fn update(&mut self, object: Object<'_>, urids: &Urids) -> bool {
        self.known = true;
        let places = [urids.frame, urids.bar, urids.bar_beat, urids.beat];
        let moved = object.properties().any(|property| {
            places.contains(&property.key)
                && number(urids.numbers, property.type_, property.body).is_some()
        });
        if moved {
            self.frame = None;
            self.bar = None;
            self.bar_beat = None;
            self.beat = None;
        }
        for property in object.properties() {
            let Some(value) = number(urids.numbers, property.type_, property.body) else {
                continue;
            };
            let key = property.key;
            if key == urids.frame {
                self.frame = Some(value);
            } else if key == urids.speed {
                self.speed = value;
            } else if key == urids.beats_per_minute {
                self.beats_per_minute = Some(value).filter(|&bpm| bpm > 0.0);
            } else if key == urids.beats_per_bar {
                self.beats_per_bar = Some(value).filter(|&beats| beats > 0.0);
            } else if key == urids.beat_unit {
                self.beat_unit = Some(value).filter(|&unit| unit >= 1.0);
            } else if key == urids.bar {
                self.bar = Some(value);
            } else if key == urids.bar_beat {
                self.bar_beat = Some(value);
            } else if key == urids.beat {
                self.beat = Some(value);
            }
        }
        moved
    }

    /// Takes in a `time:Position` stamped `late` frames past the end of the
    /// block, so that the position holds what the transport is at the end
    /// of the block, where the next block starts: a new place is moved back
    /// by those frames.
    fn update_late(&mut self, object: Object<'_>, late: u32, urids: &Urids, sample_rate: f64) {
        if self.update(object, urids) {
            self.advance(-f64::from(late), sample_rate);
        }
    }

    /// Moves the position on by `frames` at the transport's speed, or back
    /// when `frames` is negative.
    ///
    /// A place that stops being a finite number on the way, as it does when
    /// a host gives a huge but finite speed or tempo, is forgotten rather
    /// than handed to the plugin.
    fn advance(&mut self, frames: f64, sample_rate: f64) {
        if self.speed == 0.0 || frames == 0.0 {
            return;
        }
        let moved = frames * self.speed;
        if let Some(frame) = &mut self.frame {
            *frame += moved;
        }
        if let Some(bpm) = self.beats_per_minute {
            let beats = moved * bpm / (60.0 * sample_rate);
            if let Some(beat) = &mut self.beat {
                *beat += beats;
            }
            if let Some(bar_beat) = &mut self.bar_beat {
                *bar_beat += beats;
                if let Some(length) = self.beats_per_bar {
                    let bars = (*bar_beat / length).floor();
                    if bars != 0.0 {
                        *bar_beat -= bars * length;
                        if let Some(bar) = &mut self.bar {
                            *bar += bars;
                        }
                    }
                }
            }
        }
        for place in [
            &mut self.frame,
            &mut self.beat,
            &mut self.bar_beat,
            &mut self.bar,
        ] {
            if place.is_some_and(|value| !value.is_finite()) {
                *place = None;
            }
        }
    }

    /// The framework's transport. LV2 counts beats of `time:beatUnit` and
    /// the framework counts quarter notes, so tempo and positions are
    /// scaled by 4 over the beat unit when it is known.
    ///
    /// The start of the bar is `time:beat` minus `time:barBeat` when the
    /// host gives both, which stays right after the time signature has
    /// changed; only without the running beat is it worked out as
    /// `time:bar` times `time:beatsPerBar`, which assumes the signature
    /// never changed. A value that does not come out finite is left
    /// unknown.
    fn transport(&self) -> Transport {
        if !self.known {
            return Transport::STOPPED;
        }
        let quarters = self.beat_unit.map_or(1.0, |unit| 4.0 / unit);
        let finite = |beats: f64| Some(beats * quarters).filter(|beats| beats.is_finite());
        let in_bars = match (self.bar, self.beats_per_bar) {
            (Some(bar), Some(length)) => Some(bar * length),
            _ => None,
        };
        let position = self
            .beat
            .or_else(|| Some(in_bars? + self.bar_beat?))
            .and_then(finite);
        let bar_start = self
            .beat
            .zip(self.bar_beat)
            .map(|(beat, bar_beat)| beat - bar_beat)
            .or(in_bars)
            .and_then(finite);
        let time_signature = match (self.beats_per_bar, self.beat_unit) {
            (Some(beats), Some(unit)) if beats <= f64::from(u16::MAX) && unit <= 65_535.0 => {
                Some((beats.round().max(1.0) as u16, unit.round() as u16))
            }
            _ => None,
        };
        Transport {
            playing: self.speed != 0.0,
            recording: false,
            tempo_bpm: self.beats_per_minute.and_then(finite),
            time_signature,
            position_samples: self.frame.map(|frame| frame.floor() as i64),
            position_beats: position,
            bar_start_beats: bar_start,
            loop_beats: None,
        }
    }
}

/// The time stamps of a sequence, made usable: frames counted from the
/// block's start, never negative and never going back. A sequence in beats
/// has every event on frame 0.
#[derive(Debug, Clone)]
struct Timeline<'a> {
    events: Events<'a>,
    in_frames: bool,
    last: u32,
}

impl<'a> Timeline<'a> {
    fn new(sequence: Sequence<'a>, urids: &Urids) -> Self {
        Self {
            events: sequence.events(),
            in_frames: sequence.unit == 0 || sequence.unit == urids.frame_time,
            last: 0,
        }
    }

    /// The next event with its frame.
    fn next(&mut self) -> Option<(u32, crate::atom::AtomEvent<'a>)> {
        let event = self.events.next()?;
        if self.in_frames {
            let frame = u32::try_from(event.time.max(0)).unwrap_or(u32::MAX);
            self.last = self.last.max(frame);
        }
        Some((self.last, event))
    }

    /// The next `time:Position` with its frame.
    fn next_position(&mut self, urids: &Urids) -> Option<(u32, Object<'a>)> {
        while let Some((frame, event)) = self.next() {
            if !urids.is_object(event.type_) {
                continue;
            }
            if let Some(object) = Object::read(event.body).filter(|o| o.otype == urids.position) {
                return Some((frame, object));
            }
        }
        None
    }
}

// --- Instance -----------------------------------------------------------------

/// Where the host put each port's data. Null until connected.
struct Connections {
    audio_in: Box<[*const f32]>,
    sidechain: Box<[*const f32]>,
    audio_out: Box<[*mut f32]>,
    control: *const u8,
    notify: *mut u8,
    latency: *mut f32,
    params: Box<[*const f32]>,
}

/// One plugin as the host sees it.
struct Instance<P: Plugin> {
    params: Arc<P::Params>,
    urids: Urids,
    log: Option<Log>,
    sample_rate: f64,
    /// The longest piece processed at once, as the plugin was prepared for.
    max_block: usize,
    ports: PortLayout,
    /// See the module documentation for who may touch this and when.
    audio: UnsafeCell<Audio<P>>,
}

/// The state used while processing.
struct Audio<P> {
    plugin: P,
    /// Whether `activate` ran, and `deactivate` has not since.
    active: bool,
    /// Latency the plugin last reported, for the latency port.
    latency: f32,
    connections: Connections,
    /// The value last read from each parameter's control port, in
    /// parameter order; not a number before the first read.
    port_values: Box<[f32]>,
    buffer: AudioBuffer<f32>,
    sidechain: AudioBuffer<f32>,
    /// The events of the block.
    events: EventBuffer,
    /// The events of the piece being processed, counted from its start.
    piece: EventBuffer,
    /// Parameter values as the plugin last saw them.
    watcher: ParamWatcher,
    position: Position,
    /// Whether the plugin panicked in the last block, so that the next
    /// starts by giving it every parameter's value.
    resend_values: bool,
}

/// The instance behind a handle the host passed back.
///
/// # Safety
///
/// `handle` must be null or a handle returned by `instantiate::<P>` that has
/// not been cleaned up.
unsafe fn instance<'a, P: Plugin>(handle: LV2_Handle) -> Option<&'a Instance<P>> {
    // SAFETY: per this function's contract.
    unsafe { handle.cast::<Instance<P>>().as_ref() }
}

unsafe extern "C" fn instantiate<P: Plugin>(
    _descriptor: *const LV2_Descriptor,
    sample_rate: f64,
    _bundle_path: *const c_char,
    features: *const *const LV2_Feature,
) -> LV2_Handle {
    guard(ptr::null_mut(), || {
        // SAFETY: the host passes its null-terminated feature list.
        let features = unsafe { Features::read(features) };
        // `urid:map` is required: without it no atom can be read.
        let Some(map) = features.map else {
            return ptr::null_mut();
        };
        // SAFETY: the data of the host's `urid:map` feature.
        let Some(urids) = (unsafe { Urids::new(&map) }) else {
            return ptr::null_mut();
        };
        // SAFETY: the data of the host's `log:log` and `urid:map` features.
        let log_feature = features.log.and_then(|log| unsafe { Log::new(log, &map) });
        let log = log_feature.as_ref();
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            self::log(log, Level::Error, "the host gave no usable sample rate");
            return ptr::null_mut();
        }
        // SAFETY: the host's terminated option list, or null.
        let bound = unsafe { max_block_length(features.options, &urids) };
        let max_block = match bound {
            Some(frames) if frames > 0 => {
                let frames = usize::try_from(frames).unwrap_or(usize::MAX);
                if frames > LARGEST_MAX_BLOCK {
                    self::log(
                        log,
                        Level::Note,
                        &format!(
                            "the host's maximum block length of {frames} frames is processed \
                             in pieces of {LARGEST_MAX_BLOCK}"
                        ),
                    );
                }
                frames.min(LARGEST_MAX_BLOCK)
            }
            _ => {
                self::log(
                    log,
                    Level::Note,
                    &format!(
                        "the host gave no maximum block length; longer blocks are processed \
                         in pieces of {DEFAULT_MAX_BLOCK} frames"
                    ),
                );
                DEFAULT_MAX_BLOCK
            }
        };

        let plugin = P::new();
        let params = plugin.params();
        if let Err(error) = params.validate() {
            self::log(
                log,
                Level::Error,
                &format!("the plugin's parameters clash: {error}"),
            );
            return ptr::null_mut();
        }
        let count = params.len();
        let layout = P::LAYOUT;
        let instance = Instance::<P> {
            urids,
            log: log_feature,
            sample_rate,
            max_block,
            ports: PortLayout::new(layout, count),
            audio: UnsafeCell::new(Audio {
                plugin,
                active: false,
                latency: 0.0,
                connections: Connections {
                    audio_in: vec![ptr::null(); layout.inputs].into_boxed_slice(),
                    sidechain: vec![ptr::null(); layout.sidechain].into_boxed_slice(),
                    audio_out: vec![ptr::null_mut(); layout.outputs].into_boxed_slice(),
                    control: ptr::null(),
                    notify: ptr::null_mut(),
                    latency: ptr::null_mut(),
                    params: vec![ptr::null(); count].into_boxed_slice(),
                },
                port_values: vec![f32::NAN; count].into_boxed_slice(),
                buffer: AudioBuffer::new(layout.outputs, max_block),
                sidechain: AudioBuffer::new(layout.sidechain, max_block),
                events: EventBuffer::with_capacity(EVENTS_PER_BLOCK),
                piece: EventBuffer::with_capacity(EVENTS_PER_BLOCK),
                watcher: ParamWatcher::new(&*params),
                position: Position::default(),
                resend_values: false,
            }),
            params,
        };
        Box::into_raw(Box::new(instance)).cast()
    })
}

unsafe extern "C" fn connect_port<P: Plugin>(handle: LV2_Handle, port: u32, data: *mut c_void) {
    guard((), || {
        // SAFETY: the host passes a handle it got from `instantiate`.
        let Some(instance) = (unsafe { instance::<P>(handle) }) else {
            return;
        };
        // SAFETY: `connect_port` is never called at the same time as `run`
        // or another `connect_port`, the only other users of this state.
        let connections = unsafe { &mut (*instance.audio.get()).connections };
        let Some(role) = usize::try_from(port)
            .ok()
            .and_then(|index| instance.ports.role(index))
        else {
            return;
        };
        let slot = match role {
            PortRole::AudioInput(channel) => connections.audio_in.get_mut(channel),
            PortRole::Sidechain(channel) => connections.sidechain.get_mut(channel),
            PortRole::Param(index) => connections.params.get_mut(index),
            PortRole::AudioOutput(channel) => {
                if let Some(slot) = connections.audio_out.get_mut(channel) {
                    *slot = data.cast();
                }
                return;
            }
            PortRole::Control => {
                connections.control = data.cast();
                return;
            }
            PortRole::Notify => {
                connections.notify = data.cast();
                return;
            }
            PortRole::Latency => {
                connections.latency = data.cast();
                return;
            }
        };
        if let Some(slot) = slot {
            *slot = data.cast_const().cast();
        }
    });
}

unsafe extern "C" fn activate<P: Plugin>(handle: LV2_Handle) {
    guard((), || {
        // SAFETY: the host passes a handle it got from `instantiate`.
        let Some(instance) = (unsafe { instance::<P>(handle) }) else {
            return;
        };
        // SAFETY: `activate` is never called at the same time as any other
        // function of the instance.
        let audio = unsafe { &mut *instance.audio.get() };
        audio.active = false;
        audio.plugin.prepare(&ProcessSpec {
            sample_rate: instance.sample_rate,
            max_block_size: instance.max_block,
            num_channels: P::LAYOUT.outputs,
        });
        audio.latency = audio.plugin.latency_samples() as f32;
        audio.position = Position::default();
        audio.active = true;
    });
}

unsafe extern "C" fn deactivate<P: Plugin>(handle: LV2_Handle) {
    guard((), || {
        // SAFETY: the host passes a handle it got from `instantiate`.
        let Some(instance) = (unsafe { instance::<P>(handle) }) else {
            return;
        };
        // SAFETY: as in `activate`.
        let audio = unsafe { &mut *instance.audio.get() };
        if audio.active {
            audio.active = false;
            audio.plugin.reset();
        }
    });
}

unsafe extern "C" fn cleanup<P: Plugin>(handle: LV2_Handle) {
    guard((), || {
        if handle.is_null() {
            return;
        }
        // SAFETY: the handle came from `Box::into_raw` in `instantiate`, and
        // the host cleans each instance up once, when nothing else is
        // calling it.
        drop(unsafe { Box::from_raw(handle.cast::<Instance<P>>()) });
    });
}

unsafe extern "C" fn extension_data<P: Plugin>(uri: *const c_char) -> *const c_void {
    if uri.is_null() {
        return ptr::null();
    }
    // SAFETY: the host passes a NUL-terminated URI.
    let uri = unsafe { CStr::from_ptr(uri) };
    if uri == LV2_STATE__interface {
        ptr::from_ref(Vtables::<P>::STATE).cast()
    } else {
        ptr::null()
    }
}

/// The tables of functions handed to the host, one set per plugin type.
struct Vtables<P>(PhantomData<P>);

impl<P: Plugin> Vtables<P> {
    const STATE: &'static LV2_State_Interface = &LV2_State_Interface {
        save: Some(state_save::<P>),
        restore: Some(state_restore::<P>),
    };
}

// --- Processing ---------------------------------------------------------------

unsafe extern "C" fn run<P: Plugin>(handle: LV2_Handle, sample_count: u32) {
    // Numbers too small to hear are zero for as long as the plugin runs,
    // and the host's own setting is back when this returns.
    let _quiet = NoDenormals::new();
    // SAFETY: the host passes a handle it got from `instantiate`.
    let Some(instance) = (unsafe { instance::<P>(handle) }) else {
        return;
    };
    let audio = instance.audio.get();
    let frames = sample_count as usize;
    let finished = guard(false, || {
        // SAFETY: `run` is never called at the same time as itself or
        // `connect_port`, the only other users of this state while active.
        let audio = unsafe { &mut *audio };
        // SAFETY: the ports the host connected are valid for this block.
        unsafe { instance.run(audio, frames) };
        true
    });
    if !finished {
        // The plugin panicked. The host hears silence rather than whatever
        // was half written, and the plugin gets the next block as usual,
        // starting with every parameter's value, since the changes of this
        // block may not have reached it.
        // SAFETY: as above; the borrow inside `guard` has ended.
        unsafe {
            (*audio).resend_values = true;
            silence(&(*audio).connections, 0, frames);
        }
    }
}

/// Fills frames `start..start + frames` of every connected output with
/// silence.
///
/// # Safety
///
/// Every non-null output must have room for `start + frames` samples.
unsafe fn silence(connections: &Connections, start: usize, frames: usize) {
    for &port in &connections.audio_out {
        if !port.is_null() {
            // SAFETY: per this function's contract.
            unsafe { std::slice::from_raw_parts_mut(port.add(start), frames) }.fill(0.0);
        }
    }
}

/// The host's control sequence as bytes, the size its header gives.
///
/// # Safety
///
/// `port` must be null or point to an atom whose header gives the size of
/// the body that follows it.
unsafe fn atom_bytes<'a>(port: *const u8) -> Option<&'a [u8]> {
    if port.is_null() {
        return None;
    }
    // SAFETY: the atom header is readable.
    let size = unsafe { port.cast::<u32>().read_unaligned() };
    let len = usize::try_from(size)
        .ok()?
        .checked_add(size_of::<LV2_Atom>())?;
    // SAFETY: the header and the body it describes are readable.
    Some(unsafe { std::slice::from_raw_parts(port, len) })
}

/// Writes an empty sequence into the host's notify buffer, if there is
/// room. The host sets the header's size to the room it has.
///
/// # Safety
///
/// `port` must be null or point to an atom header followed by as many
/// writable bytes as its size gives.
unsafe fn write_empty_sequence(port: *mut u8, urids: &Urids) {
    if port.is_null() {
        return;
    }
    // SAFETY: the atom header is readable.
    let room = unsafe { port.cast::<u32>().read_unaligned() } as usize;
    if room < size_of::<LV2_Atom_Sequence_Body>() {
        return;
    }
    let empty = LV2_Atom_Sequence {
        atom: LV2_Atom {
            size: size_of::<LV2_Atom_Sequence_Body>() as u32,
            type_: urids.sequence,
        },
        body: LV2_Atom_Sequence_Body {
            unit: urids.frame_time,
            pad: 0,
        },
    };
    // SAFETY: the header and `room` bytes after it are writable, and the
    // sequence body fits in them.
    unsafe { port.cast::<LV2_Atom_Sequence>().write_unaligned(empty) };
}

impl<P: Plugin> Instance<P> {
    /// Processes one block. Real-time safe.
    ///
    /// # Safety
    ///
    /// Every connected port must be valid for a block of `frames` frames.
    unsafe fn run(&self, audio: &mut Audio<P>, frames: usize) {
        // SAFETY: the ports are valid for this block, per our contract.
        unsafe {
            write_empty_sequence(audio.connections.notify, &self.urids);
            if !audio.connections.latency.is_null() {
                audio.connections.latency.write_unaligned(audio.latency);
            }
        }
        if !audio.active {
            // SAFETY: as above.
            unsafe { silence(&audio.connections, 0, frames) };
            return;
        }
        // SAFETY: as above.
        let sequence = unsafe { atom_bytes(audio.connections.control) }
            .and_then(|bytes| Sequence::read(bytes, self.urids.sequence));

        if frames == 0 {
            // Nothing to play the events on. Parameters are stored, and the
            // next block announces them; the transport moves, every
            // position being past the end of this block; MIDI is dropped.
            // SAFETY: as above.
            unsafe { self.read_param_ports(audio, true) };
            if let Some(sequence) = sequence {
                let mut timeline = Timeline::new(sequence, &self.urids);
                while let Some((frame, object)) = timeline.next_position(&self.urids) {
                    audio
                        .position
                        .update_late(object, frame, &self.urids, self.sample_rate);
                }
            }
            return;
        }

        let params: &dyn ParamSet = &*self.params;
        audio.events.clear();
        // Changes the plugin has not seen yet go first, on frame 0. After a
        // block the plugin did not finish, that is every value.
        if audio.resend_values {
            let events = &mut audio.events;
            params.visit(&mut |param| {
                let kind = EventKind::ParamValue {
                    param: param.param_id(),
                    value: param.plain(),
                };
                let _ = events.push(Event { offset: 0, kind });
            });
            audio.watcher.remember(params);
            audio.resend_values = false;
        } else {
            audio.watcher.emit_changes(params, &mut audio.events);
        }
        let announced = audio.events.len();
        // SAFETY: as above.
        unsafe { self.read_param_ports(audio, false) };
        if let Some(sequence) = sequence {
            let mut timeline = Timeline::new(sequence, &self.urids);
            while let Some((frame, event)) = timeline.next() {
                if event.type_ != self.urids.midi_event {
                    continue;
                }
                if let Some(message) = MidiMessage::from_bytes(event.body) {
                    // A full buffer drops the event; allocating is not an
                    // option.
                    let _ = audio.events.push(Event::from_midi(frame, message));
                }
            }
        }
        // Only the host's values are stored: the announced ones were read
        // from the parameters.
        audio
            .watcher
            .apply_and_remember(params, &audio.events.as_slice()[announced..]);

        // SAFETY: as above.
        unsafe { self.render(audio, frames, sequence) };
        audio.latency = audio.plugin.latency_samples() as f32;
        // SAFETY: as above.
        unsafe {
            if !audio.connections.latency.is_null() {
                audio.connections.latency.write_unaligned(audio.latency);
            }
        }
    }

    /// Reads every parameter's control port. A port whose value changed
    /// since the last read, to something the parameter does not hold
    /// already, becomes a parameter event on frame 0, or with `quietly` is
    /// stored straight into the parameter for the next block to announce.
    ///
    /// # Safety
    ///
    /// Every connected control port must be readable.
    unsafe fn read_param_ports(&self, audio: &mut Audio<P>, quietly: bool) {
        let Audio {
            connections,
            port_values,
            events,
            ..
        } = audio;
        let mut index = 0;
        self.params.visit(&mut |param| {
            let at = index;
            index += 1;
            let (Some(&port), Some(seen)) = (connections.params.get(at), port_values.get_mut(at))
            else {
                return;
            };
            if port.is_null() {
                return;
            }
            // SAFETY: a connected control port holds one `f32`.
            let value = unsafe { port.read_unaligned() };
            if value.to_bits() == seen.to_bits() {
                return;
            }
            *seen = value;
            if value.is_nan() {
                return;
            }
            // The value the parameter will hold, clamped and snapped, is what
            // the event carries.
            let Some(stored) = param.stored_plain(f64::from(value)) else {
                return;
            };
            if stored == param.plain() {
                return;
            }
            if quietly {
                param.set_plain(stored);
            } else {
                let kind = EventKind::ParamValue {
                    param: param.param_id(),
                    value: stored,
                };
                // A full buffer leaves the port's value for the next block.
                if events.push(Event { offset: 0, kind }).is_err() {
                    *seen = f32::NAN;
                }
            }
        });
    }

    /// Processes the block in pieces: at each `time:Position` inside it,
    /// and at most `max_block` frames at a time.
    ///
    /// # Safety
    ///
    /// Every connected audio port must be valid for `frames` frames.
    unsafe fn render(&self, audio: &mut Audio<P>, frames: usize, sequence: Option<Sequence<'_>>) {
        let urids = &self.urids;
        let mut timeline = sequence.map(|sequence| Timeline::new(sequence, urids));
        let mut next_position = timeline
            .as_mut()
            .and_then(|timeline| timeline.next_position(urids));
        let mut start = 0;
        let mut delivered = 0;
        loop {
            // The transport changes that take effect here.
            while let Some((frame, object)) = next_position {
                if frame as usize > start {
                    break;
                }
                audio.position.update(object, urids);
                next_position = timeline
                    .as_mut()
                    .and_then(|timeline| timeline.next_position(urids));
            }
            if start >= frames {
                break;
            }
            let mut end = frames.min(start + self.max_block);
            if let Some((frame, _)) = next_position {
                end = end.min(frame as usize);
            }

            // The events of this piece, counted from its start. The last
            // piece also takes those past the end of the block, which the
            // plugin handles after its last frame.
            audio.piece.clear();
            while let Some(&event) = audio.events.as_slice().get(delivered) {
                if end < frames && event.offset as usize >= end {
                    break;
                }
                let offset = event.offset.saturating_sub(start as u32);
                let _ = audio.piece.push(Event { offset, ..event });
                delivered += 1;
            }

            // SAFETY: the audio ports are valid for `frames` frames, and
            // `start..end` lies within them.
            unsafe { self.process_piece(audio, start, end) };
            audio
                .position
                .advance((end - start) as f64, self.sample_rate);
            start = end;
        }
        // Positions stamped past the block take effect for the next one,
        // moved back to where that block starts.
        while let Some((frame, object)) = next_position {
            let late = frame.saturating_sub(u32::try_from(frames).unwrap_or(u32::MAX));
            audio
                .position
                .update_late(object, late, urids, self.sample_rate);
            next_position = timeline
                .as_mut()
                .and_then(|timeline| timeline.next_position(urids));
        }
    }

    /// Processes frames `start..end` of the block.
    ///
    /// # Safety
    ///
    /// Every connected audio port must be valid for `end` frames.
    unsafe fn process_piece(&self, audio: &mut Audio<P>, start: usize, end: usize) {
        let frames = end - start;
        audio.buffer.set_num_frames(frames);
        audio.buffer.clear();
        let inputs = P::LAYOUT.inputs.min(audio.buffer.num_channels());
        for (channel, &port) in audio.connections.audio_in.iter().enumerate().take(inputs) {
            if !port.is_null() {
                // SAFETY: per this function's contract.
                let samples = unsafe { std::slice::from_raw_parts(port.add(start), frames) };
                audio.buffer.channel_mut(channel).copy_from_slice(samples);
            }
        }
        if P::LAYOUT.sidechain > 0 {
            audio.sidechain.set_num_frames(frames);
            audio.sidechain.clear();
            for (channel, &port) in audio.connections.sidechain.iter().enumerate() {
                if !port.is_null() {
                    // SAFETY: per this function's contract.
                    let samples = unsafe { std::slice::from_raw_parts(port.add(start), frames) };
                    audio
                        .sidechain
                        .channel_mut(channel)
                        .copy_from_slice(samples);
                }
            }
        }

        let transport = audio.position.transport();
        let mut context = ProcessContext::new(&audio.piece, &transport);
        if P::LAYOUT.sidechain > 0 {
            context = context.with_sidechain(&audio.sidechain);
        }
        audio.plugin.process(&mut audio.buffer, &context);

        // Everything was read from the inputs before this point, so writing
        // is sound even when the host processes in place.
        for (channel, &port) in audio.connections.audio_out.iter().enumerate() {
            if port.is_null() {
                continue;
            }
            // SAFETY: per this function's contract, and no reference to the
            // host's input memory is alive here.
            let out = unsafe { std::slice::from_raw_parts_mut(port.add(start), frames) };
            match (channel < audio.buffer.num_channels()).then(|| audio.buffer.channel(channel)) {
                Some(samples) => out.copy_from_slice(samples),
                None => out.fill(0.0),
            }
        }
    }
}

// --- State ----------------------------------------------------------------------

unsafe extern "C" fn state_save<P: Plugin>(
    handle: LV2_Handle,
    store: LV2_State_Store_Function,
    store_handle: LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    guard(LV2_STATE_ERR_UNKNOWN, || {
        // SAFETY: the host passes a handle it got from `instantiate`.
        let (Some(instance), Some(store)) = (unsafe { instance::<P>(handle) }, store) else {
            return LV2_STATE_ERR_UNKNOWN;
        };
        let mut text = save_state::<P>(&instance.params).into_bytes();
        if text.contains(&0) {
            return LV2_STATE_ERR_UNKNOWN;
        }
        // An `atom:String` counts its terminating NUL.
        text.push(0);
        // SAFETY: the host's own function with the host's own handle, and a
        // value of exactly the size passed, which the host copies.
        unsafe {
            store(
                store_handle,
                instance.urids.state_key,
                text.as_ptr().cast(),
                text.len(),
                instance.urids.string,
                LV2_STATE_IS_POD | LV2_STATE_IS_PORTABLE,
            )
        }
    })
}

unsafe extern "C" fn state_restore<P: Plugin>(
    handle: LV2_Handle,
    retrieve: LV2_State_Retrieve_Function,
    retrieve_handle: LV2_State_Handle,
    _flags: u32,
    _features: *const *const LV2_Feature,
) -> LV2_State_Status {
    guard(LV2_STATE_ERR_UNKNOWN, || {
        // SAFETY: the host passes a handle it got from `instantiate`.
        let (Some(instance), Some(retrieve)) = (unsafe { instance::<P>(handle) }, retrieve) else {
            return LV2_STATE_ERR_UNKNOWN;
        };
        let refuse = |status, why: &str| {
            log(
                instance.log.as_ref(),
                Level::Warning,
                &format!("state not restored: {why}"),
            );
            status
        };
        let (mut size, mut type_, mut flags) = (0usize, 0u32, 0u32);
        // SAFETY: the host's own function with the host's own handle, and
        // places for the three results.
        let value = unsafe {
            retrieve(
                retrieve_handle,
                instance.urids.state_key,
                &mut size,
                &mut type_,
                &mut flags,
            )
        };
        if value.is_null() {
            return LV2_STATE_ERR_NO_PROPERTY;
        }
        if type_ != instance.urids.string {
            return refuse(LV2_STATE_ERR_BAD_TYPE, "the value is not a string");
        }
        if size > MAX_STATE_BYTES {
            return refuse(LV2_STATE_ERR_UNKNOWN, "the value is too large");
        }
        // SAFETY: the host's value holds `size` bytes and stays valid until
        // `restore` returns.
        let bytes = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), size) };
        let text = bytes.split(|&byte| byte == 0).next().unwrap_or_default();
        let Ok(text) = std::str::from_utf8(text) else {
            return refuse(LV2_STATE_ERR_UNKNOWN, "the value is not UTF-8");
        };
        // The parameters change without an event; the next block announces
        // them to the plugin.
        match load_state::<P>(&instance.params, text) {
            Ok(_) => LV2_STATE_SUCCESS,
            Err(error) => refuse(LV2_STATE_ERR_UNKNOWN, &error.to_string()),
        }
    })
}
