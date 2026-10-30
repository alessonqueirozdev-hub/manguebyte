//! The glue between the Audio Unit's C interface and a [`Plugin`].
//!
//! # Threads
//!
//! Apple's interface leaves most of the threading to convention, so the
//! wrapper keeps its state where the audio thread never waits for it and
//! never touches a system lock:
//!
//! - Parameters are atomics, so `GetParameter`, `SetParameter`, saving the
//!   state and the parameter properties work from any thread at any time.
//! - The configuration (formats, connections, the host's callbacks), the
//!   audio state (the plugin, its buffers, the events waiting for the next
//!   render) and the render notifications each sit in an
//!   [`Exclusive`]: a flag taken with one
//!   compare-and-swap, not a mutex, which on macOS allocates the first time
//!   it is locked. `Initialize`, `Uninitialize`, `Reset` and the properties
//!   take them, yielding until they are free. `Render`,
//!   `ScheduleParameters` and `MusicDeviceMIDIEvent`, which run on the
//!   audio thread, only ever try them: should another thread hold one at
//!   that moment, which a host keeping to the conventions never makes
//!   happen, the slice is silent or the event is dropped and counted,
//!   and nothing waits.
//! - Property listeners live behind a mutex, which only the threads that
//!   change properties take, and are called with nothing held; render
//!   notifications are copied out and called with nothing held, so that
//!   either may call back into the unit.
//!
//! # One event stream
//!
//! Each render gives the plugin one stream of events, as the other
//! wrappers do: first, on frame 0, every parameter that changed without an
//! event (from `SetParameter`, an editor or a restored state), then the
//! parameter changes `ScheduleParameters` placed on their frames and the
//! MIDI `MusicDeviceMIDIEvent` placed on its frames, in time order.

// The constants keep Apple's names, which are not upper case.
#![allow(non_upper_case_globals)]

use std::ffi::{c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use manguebyte_core::{AudioBuffer, NoDenormals, ParamId};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Transport};
use manguebyte_midi::MidiMessage;
use manguebyte_params::{Param, ParamSet};
use manguebyte_plugin::output::{OutputBuffer, OutputProtocol, OutputTranslator};
use manguebyte_plugin::{
    AudioLayout, AuxOutputs, ParamWatcher, Plugin, PluginKind, load_state, offered_layouts,
    plugin_info, save_state,
};

use crate::cf::{self, Owned};
use crate::codes::{as_os_type, codes};
use crate::exclusive::Exclusive;
use crate::sys::AudioBuffer as AuBuffer;
use crate::sys::*;

/// The sample rate an instance starts at, before the host sets one.
pub const DEFAULT_SAMPLE_RATE: f64 = 44_100.0;

/// The longest slice an instance renders before the host sets its own,
/// as Apple's own base classes have it.
pub const DEFAULT_MAX_FRAMES: u32 = 1_156;

/// The longest slice a host may ask for, so that its request cannot make
/// the unit allocate without limit.
pub const LARGEST_MAX_FRAMES: u32 = 1 << 16;

/// Most events delivered to the plugin in one render. More are dropped
/// rather than allocated for.
pub const EVENTS_PER_BLOCK: usize = 4_096;

/// Largest state accepted from a host.
const MAX_STATE_BYTES: usize = 64 << 20;

/// Most property listeners and render notifications kept.
const MAX_LISTENERS: usize = 1_024;
const MAX_RENDER_NOTIFICATIONS: usize = 64;

/// How far apart the points are that stand for a scheduled ramp.
const RAMP_STEP: i64 = 32;

/// The room for one `MIDIPacketList` of the plugin's output, in 4-byte
/// words. A render that sends more hands the host several lists.
const MIDI_LIST_WORDS: usize = 16_384;

/// Runs `work`, turning a panic into `fallback`: a panic must not unwind
/// into the host.
fn guard<R>(fallback: R, work: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(work)).unwrap_or(fallback)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// --- What an instance is -------------------------------------------------------

/// Where an input element's audio comes from.
#[derive(Debug, Clone, Copy)]
enum Source {
    None,
    Callback(AURenderCallbackStruct),
    Connection(AudioUnitConnection),
}

/// What the host has set up.
struct Config {
    sample_rate: f64,
    max_frames: u32,
    /// One per input element.
    sources: [Source; 2],
    host: Option<HostCallbackInfo>,
    in_place: u32,
    bypass: bool,
    /// The elements' widths as the host set their stream formats, which
    /// must be one of the layouts the plugin offers when it initialises.
    layout: AudioLayout,
    /// Where MIDI the plugin sends goes (`kAudioUnitProperty_MIDIOutputCallback`).
    midi_out: Option<AUMIDIOutputCallbackStruct>,
}

/// What a render reads of the [`Config`], copied so that the configuration
/// is not held while the host's callbacks run.
struct Rendering {
    sources: [Source; 2],
    host: Option<HostCallbackInfo>,
    bypass: bool,
    midi_out: Option<AUMIDIOutputCallbackStruct>,
}

/// One input element's memory, which its source renders into.
struct Pull {
    storage: AudioBuffer<f32>,
    /// An `AudioBufferList` with one buffer per channel, in 8-byte words.
    list: Box<[u64]>,
}

/// What `Initialize` allocates.
struct Prepared {
    max_frames: u32,
    /// The layout the plugin was prepared for.
    layout: AudioLayout,
    buffer: AudioBuffer<f32>,
    sidechain: AudioBuffer<f32>,
    pulls: Vec<Pull>,
    /// The auxiliary output elements, all on: an Audio Unit host cannot
    /// switch an element off.
    aux: AuxOutputs,
    /// For a plugin with auxiliary elements, the sample time and length of
    /// the slice last rendered, which a render of another element for the
    /// same slice reads instead of running the plugin again.
    rendered: Option<(f64, u32)>,
}

/// The state used while rendering.
struct Audio<P> {
    plugin: P,
    prepared: Option<Prepared>,
    /// Events for the next render, on their frames.
    pending: EventBuffer,
    /// The events of the render in progress.
    events: EventBuffer,
    watcher: ParamWatcher,
    /// What the plugin sends during a render.
    output: OutputBuffer,
    /// Turns it into MIDI 1.0 for the host.
    translator: OutputTranslator,
    /// One `MIDIPacketList` being written, in words so that it is aligned
    /// as CoreMIDI's structures are.
    packets: Box<[u32]>,
}

/// The preset a host says is the present one: Apple's base classes start
/// with number -1, a user's preset, named "Untitled".
struct Preset {
    number: i32,
    /// The name as text, for the state's dictionary.
    text: String,
    /// The same as a string of the unit's own, which
    /// `kAudioUnitProperty_CurrentPreset` hands out without a reference
    /// for the caller, so it has to live here. `None` only if
    /// CoreFoundation could not make one.
    name: Option<Owned>,
}

/// What a preset is called when nobody named it.
const UNTITLED: &str = "Untitled";

impl Preset {
    fn untitled() -> Self {
        Self::named(-1, UNTITLED)
    }

    fn named(number: i32, text: &str) -> Self {
        Self {
            number,
            text: text.to_owned(),
            name: cf::string(text),
        }
    }

    /// The preset as `AUPreset`. With `retained`, the name carries a
    /// reference the caller releases (`kAudioUnitProperty_PresentPreset`);
    /// without, it is the unit's own, valid until the preset changes
    /// (`kAudioUnitProperty_CurrentPreset`).
    fn to_c(&self, retained: bool) -> AUPreset {
        let name = self.name.as_ref().map_or(ptr::null(), Owned::as_ptr);
        if retained && !name.is_null() {
            // SAFETY: a live string this value holds; the reference taken
            // here is the caller's.
            unsafe { CFRetain(name) };
        }
        AUPreset {
            presetNumber: self.number,
            presetName: name,
        }
    }
}

#[derive(Clone, Copy)]
struct Listener {
    property: AudioUnitPropertyID,
    proc_: AudioUnitPropertyListenerProc,
    user: *mut c_void,
}

#[derive(Clone, Copy)]
struct Notification {
    proc_: AURenderCallback,
    user: *mut c_void,
}

impl Notification {
    /// Whether this is the notification `proc_` with `user`. A host removes
    /// one by naming the function it added, so functions are compared by
    /// address, which is how the host knows them too.
    fn is(&self, proc_: AURenderCallback, user: *mut c_void) -> bool {
        let same = match (self.proc_, proc_) {
            (Some(a), Some(b)) => std::ptr::fn_addr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        same && self.user == user
    }
}

/// One Audio Unit instance, as the factory makes it.
///
/// The interface comes first, so that the pointer the host is given is a
/// pointer to it, as `AudioComponentPlugInInterface` requires.
#[repr(C)]
pub(crate) struct Instance<P: Plugin> {
    interface: AudioComponentPlugInInterface,
    /// The instance the host made, for property listeners.
    component: AtomicPtr<c_void>,
    params: Arc<P::Params>,
    /// The parameters' ids, in the order the set visits them.
    ids: Box<[u32]>,
    /// Type, subtype and manufacturer.
    codes: [u32; 3],
    /// The channels of each input element in the plugin's own layout: the
    /// main input, then the sidechain, each if the plugin has it.
    inputs: Box<[usize]>,
    /// The layouts the elements' formats may add up to, the plugin's own
    /// first.
    layouts: Vec<AudioLayout>,
    initialized: AtomicBool,
    latency: AtomicU32,
    last_error: AtomicI32,
    /// Events dropped on the audio thread, for the tests.
    dropped: AtomicU64,
    config: Exclusive<Config>,
    audio: Exclusive<Audio<P>>,
    /// Only ever taken off the audio thread.
    listeners: Mutex<Vec<Listener>>,
    /// The present preset; only ever taken off the audio thread.
    preset: Mutex<Preset>,
    notifications: Exclusive<Vec<Notification>>,
}

fn takes_midi<P: Plugin>() -> bool {
    matches!(
        P::INFO.kind,
        PluginKind::Instrument | PluginKind::NoteEffect
    )
}

/// Makes an instance and hands the host its interface, or null if the
/// plugin cannot be made. Used by [`export_au!`](crate::export_au).
pub(crate) fn factory<P: Plugin>() -> *mut c_void {
    guard(ptr::null_mut(), || {
        let plugin = P::new();
        let params = plugin.params();
        if params.validate().is_err() {
            return ptr::null_mut();
        }
        let mut ids = Vec::with_capacity(params.len());
        params.visit(&mut |param| ids.push(param.param_id().0));
        let layout = P::LAYOUT;
        let inputs: Vec<usize> = [layout.inputs, layout.sidechain]
            .into_iter()
            .filter(|&channels| channels > 0)
            .collect();
        let instance = Instance::<P> {
            interface: AudioComponentPlugInInterface {
                Open: open::<P>,
                Close: close::<P>,
                Lookup: lookup::<P>,
                reserved: ptr::null_mut(),
            },
            component: AtomicPtr::new(ptr::null_mut()),
            ids: ids.into_boxed_slice(),
            codes: codes(plugin_info::<P>()).map(as_os_type),
            inputs: inputs.into_boxed_slice(),
            layouts: offered_layouts::<P>(),
            initialized: AtomicBool::new(false),
            latency: AtomicU32::new(0),
            last_error: AtomicI32::new(noErr),
            dropped: AtomicU64::new(0),
            config: Exclusive::new(Config {
                sample_rate: DEFAULT_SAMPLE_RATE,
                max_frames: DEFAULT_MAX_FRAMES,
                sources: [Source::None; 2],
                host: None,
                in_place: 0,
                bypass: false,
                layout,
                midi_out: None,
            }),
            audio: Exclusive::new(Audio {
                watcher: ParamWatcher::new(&*params),
                plugin,
                prepared: None,
                pending: EventBuffer::with_capacity(EVENTS_PER_BLOCK),
                events: EventBuffer::with_capacity(EVENTS_PER_BLOCK),
                output: if P::NOTE_OUTPUT {
                    OutputBuffer::with_capacity(EVENTS_PER_BLOCK)
                } else {
                    OutputBuffer::new()
                },
                translator: OutputTranslator::new(OutputProtocol::Midi1),
                packets: vec![0; if P::NOTE_OUTPUT { MIDI_LIST_WORDS } else { 0 }]
                    .into_boxed_slice(),
            }),
            listeners: Mutex::new(Vec::new()),
            preset: Mutex::new(Preset::untitled()),
            notifications: Exclusive::new(Vec::with_capacity(MAX_RENDER_NOTIFICATIONS)),
            params,
        };
        Box::into_raw(Box::new(instance)).cast()
    })
}

/// The instance behind the pointer the host passes back.
///
/// # Safety
///
/// `this` must be null or a pointer [`factory`] returned for `P` that has
/// not been closed.
unsafe fn instance<'a, P: Plugin>(this: *mut c_void) -> Option<&'a Instance<P>> {
    // SAFETY: per this function's contract.
    unsafe { this.cast::<Instance<P>>().as_ref() }
}

unsafe extern "C" fn open<P: Plugin>(
    this: *mut c_void,
    component: AudioComponentInstance,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: the host passes the pointer the factory returned.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        this.component.store(component, Ordering::Release);
        noErr
    })
}

unsafe extern "C" fn close<P: Plugin>(this: *mut c_void) -> OSStatus {
    guard(paramErr, || {
        if this.is_null() {
            return paramErr;
        }
        // SAFETY: the pointer came from `Box::into_raw` in the factory, and
        // the host closes each instance once, when nothing else calls it.
        drop(unsafe { Box::from_raw(this.cast::<Instance<P>>()) });
        noErr
    })
}

/// The method of a selector, as the untyped pointer `Lookup` returns.
macro_rules! method {
    ($function:expr, fn($($arg:ty),* $(,)?)) => {{
        let typed: unsafe extern "C" fn($($arg),*) -> OSStatus = $function;
        typed as *const c_void
    }};
}

unsafe extern "C" fn lookup<P: Plugin>(selector: i16) -> *const c_void {
    type This = *mut c_void;
    type Id = AudioUnitPropertyID;
    type Scope = AudioUnitScope;
    type Element = AudioUnitElement;
    match selector {
        kAudioUnitInitializeSelect => method!(initialize::<P>, fn(This)),
        kAudioUnitUninitializeSelect => method!(uninitialize::<P>, fn(This)),
        kAudioUnitGetPropertyInfoSelect => method!(
            get_property_info::<P>,
            fn(This, Id, Scope, Element, *mut u32, *mut Boolean)
        ),
        kAudioUnitGetPropertySelect => method!(
            get_property::<P>,
            fn(This, Id, Scope, Element, *mut c_void, *mut u32)
        ),
        kAudioUnitSetPropertySelect => method!(
            set_property::<P>,
            fn(This, Id, Scope, Element, *const c_void, u32)
        ),
        kAudioUnitAddPropertyListenerSelect => method!(
            add_property_listener::<P>,
            fn(This, Id, AudioUnitPropertyListenerProc, *mut c_void)
        ),
        kAudioUnitRemovePropertyListenerSelect => method!(
            remove_property_listener::<P>,
            fn(This, Id, AudioUnitPropertyListenerProc)
        ),
        kAudioUnitRemovePropertyListenerWithUserDataSelect => method!(
            remove_property_listener_with_user_data::<P>,
            fn(This, Id, AudioUnitPropertyListenerProc, *mut c_void)
        ),
        kAudioUnitAddRenderNotifySelect => {
            method!(
                add_render_notify::<P>,
                fn(This, AURenderCallback, *mut c_void)
            )
        }
        kAudioUnitRemoveRenderNotifySelect => {
            method!(
                remove_render_notify::<P>,
                fn(This, AURenderCallback, *mut c_void)
            )
        }
        kAudioUnitGetParameterSelect => method!(
            get_parameter::<P>,
            fn(This, AudioUnitParameterID, Scope, Element, *mut AudioUnitParameterValue)
        ),
        kAudioUnitSetParameterSelect => method!(
            set_parameter::<P>,
            fn(This, AudioUnitParameterID, Scope, Element, AudioUnitParameterValue, u32)
        ),
        kAudioUnitScheduleParametersSelect => method!(
            schedule_parameters::<P>,
            fn(This, *const AudioUnitParameterEvent, u32)
        ),
        kAudioUnitRenderSelect => method!(
            render::<P>,
            fn(
                This,
                *mut AudioUnitRenderActionFlags,
                *const AudioTimeStamp,
                u32,
                u32,
                *mut AudioBufferList,
            )
        ),
        kAudioUnitResetSelect => method!(reset::<P>, fn(This, Scope, Element)),
        kMusicDeviceMIDIEventSelect if takes_midi::<P>() => {
            method!(midi_event::<P>, fn(This, u32, u32, u32, u32))
        }
        kMusicDeviceStartNoteSelect if takes_midi::<P>() => method!(
            start_note::<P>,
            fn(This, u32, u32, *mut u32, u32, *const MusicDeviceNoteParams)
        ),
        kMusicDeviceStopNoteSelect if takes_midi::<P>() => {
            method!(stop_note::<P>, fn(This, u32, u32, u32))
        }
        kMusicDeviceSysExSelect if takes_midi::<P>() => {
            method!(sysex, fn(This, *const u8, u32))
        }
        _ => ptr::null(),
    }
}

// --- Initialising --------------------------------------------------------------

impl<P: Plugin> Instance<P> {
    /// Calls the listeners of `property`, with no lock held.
    fn changed(
        &self,
        property: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
    ) {
        let listeners: Vec<Listener> = lock(&self.listeners)
            .iter()
            .filter(|listener| listener.property == property)
            .copied()
            .collect();
        let component = self.component.load(Ordering::Acquire);
        for listener in listeners {
            if let Some(call) = listener.proc_ {
                // SAFETY: the host's own listener and its data, called as
                // the host registered it to be.
                unsafe { call(listener.user, component, property, scope, element) };
            }
        }
    }

    /// The channels of an element of a scope as the host has set them, or
    /// the error for one that does not exist.
    fn channels(
        &self,
        scope: AudioUnitScope,
        element: AudioUnitElement,
    ) -> Result<usize, OSStatus> {
        let layout = self.config.take().layout;
        width::<P>(layout, scope, element)
    }

    /// Every width an element can be given: what it has in the layouts
    /// the plugin offers, once each, in their order.
    fn widths(&self, scope: AudioUnitScope, element: AudioUnitElement) -> Vec<usize> {
        let mut widths = Vec::new();
        for &layout in &self.layouts {
            if let Ok(channels) = width::<P>(layout, scope, element) {
                if channels > 0 && !widths.contains(&channels) {
                    widths.push(channels);
                }
            }
        }
        widths
    }

    /// The main input and output widths of the layouts offered, once each,
    /// for `kAudioUnitProperty_SupportedNumChannels`.
    fn channel_infos(&self) -> Vec<AUChannelInfo> {
        let mut infos = Vec::new();
        for layout in &self.layouts {
            let info = AUChannelInfo {
                inChannels: layout.inputs as i16,
                outChannels: layout.outputs as i16,
            };
            if !infos.contains(&info) {
                infos.push(info);
            }
        }
        infos
    }

    fn initialize(&self) -> OSStatus {
        if self.initialized.load(Ordering::Acquire) {
            return noErr;
        }
        // The elements' formats, each set on its own, must add up to a
        // layout the plugin offers.
        let layout = self.config.take().layout;
        if !self.layouts.contains(&layout) {
            return kAudioUnitErr_FormatNotSupported;
        }
        self.prepare();
        self.initialized.store(true, Ordering::Release);
        noErr
    }

    /// Prepares the plugin for the configuration as it is and allocates
    /// what rendering needs, then tells the listeners if the latency
    /// changed.
    fn prepare(&self) {
        let (sample_rate, max_frames, layout) = {
            let config = self.config.take();
            (config.sample_rate, config.max_frames, config.layout)
        };
        let frames = max_frames as usize;
        let latency = {
            let mut audio = self.audio.take();
            audio.plugin.set_layout(layout);
            audio.plugin.prepare(&ProcessSpec {
                sample_rate,
                max_block_size: frames,
                num_channels: layout.outputs,
            });
            let pulls = (0..self.inputs.len())
                .map(|element| {
                    let channels =
                        width::<P>(layout, kAudioUnitScope_Input, element as u32).unwrap_or(0);
                    pull(channels, frames)
                })
                .collect();
            let mut aux = AuxOutputs::for_plugin::<P>(frames);
            for index in 0..aux.len() {
                aux.set_active(index, true);
            }
            audio.prepared = Some(Prepared {
                max_frames,
                layout,
                buffer: AudioBuffer::new(layout.outputs, frames),
                sidechain: AudioBuffer::new(layout.sidechain, frames),
                pulls,
                aux,
                rendered: None,
            });
            audio.pending.clear();
            audio.translator.reset();
            audio.plugin.latency_samples()
        };
        let latency = u32::try_from(latency).unwrap_or(u32::MAX);
        if self.latency.swap(latency, Ordering::Relaxed) != latency {
            self.changed(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0);
        }
    }

    fn uninitialize(&self) -> OSStatus {
        let mut audio = self.audio.take();
        audio.prepared = None;
        self.initialized.store(false, Ordering::Release);
        noErr
    }
}

/// The channels an element of a scope has under `layout`, or the error for
/// an element the plugin does not have. Which elements there are is fixed
/// by the plugin's own layout: input 0 is the main input, or the sidechain
/// of an instrument; input 1 the sidechain of an effect; output 0 the main
/// output, and each output after it an auxiliary one, of fixed width.
fn width<P: Plugin>(
    layout: AudioLayout,
    scope: AudioUnitScope,
    element: AudioUnitElement,
) -> Result<usize, OSStatus> {
    let (main, keyed) = (P::LAYOUT.inputs > 0, P::LAYOUT.sidechain > 0);
    match scope {
        kAudioUnitScope_Input => match element {
            0 if main => Ok(layout.inputs),
            0 if keyed => Ok(layout.sidechain),
            1 if main && keyed => Ok(layout.sidechain),
            _ => Err(kAudioUnitErr_InvalidElement),
        },
        kAudioUnitScope_Output if element == 0 => Ok(layout.outputs),
        kAudioUnitScope_Output => P::AUX_OUTPUTS
            .get(element as usize - 1)
            .map(|aux| aux.channels)
            .ok_or(kAudioUnitErr_InvalidElement),
        _ => Err(kAudioUnitErr_InvalidScope),
    }
}

/// Sets the width of an element in `layout`, as [`width`] reads it.
fn set_width<P: Plugin>(
    layout: &mut AudioLayout,
    scope: AudioUnitScope,
    element: u32,
    channels: usize,
) {
    let main = P::LAYOUT.inputs > 0;
    match (scope, element) {
        (kAudioUnitScope_Input, 0) if main => layout.inputs = channels,
        (kAudioUnitScope_Input, _) => layout.sidechain = channels,
        (_, 0) => layout.outputs = channels,
        // An auxiliary output has one width, which `width` gives.
        _ => {}
    }
}

/// The memory of an input element of `channels` channels.
fn pull(channels: usize, frames: usize) -> Pull {
    let words = (AUDIO_BUFFER_LIST_BUFFERS + channels * size_of::<AuBuffer>()).div_ceil(8);
    Pull {
        storage: AudioBuffer::new(channels, frames),
        list: vec![0u64; words.max(1)].into_boxed_slice(),
    }
}

unsafe extern "C" fn initialize<P: Plugin>(this: *mut c_void) -> OSStatus {
    guard(kAudioUnitErr_FailedInitialization, || {
        // SAFETY: the host passes the pointer the factory returned.
        unsafe { instance::<P>(this) }.map_or(paramErr, Instance::initialize)
    })
}

unsafe extern "C" fn uninitialize<P: Plugin>(this: *mut c_void) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        unsafe { instance::<P>(this) }.map_or(paramErr, Instance::uninitialize)
    })
}

unsafe extern "C" fn reset<P: Plugin>(
    this: *mut c_void,
    _scope: AudioUnitScope,
    _element: AudioUnitElement,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        let mut audio = this.audio.take();
        audio.plugin.reset();
        audio.pending.clear();
        audio.translator.reset();
        noErr
    })
}

// --- Parameters ------------------------------------------------------------------

/// A parameter's lowest and highest values: what it stores when given
/// minus or plus infinity, which is its range's end, or else what the ends
/// of its `0..=1` positions stand for.
fn bounds(param: &dyn Param) -> (f64, f64) {
    let end = |toward: f64, normalized: f64| match param.stored_plain(toward) {
        Some(value) if value.is_finite() => value,
        _ => param.normalized_to_plain(normalized),
    };
    let (min, max) = (end(f64::NEG_INFINITY, 0.0), end(f64::INFINITY, 1.0));
    if min <= max { (min, max) } else { (max, min) }
}

/// How a host is to show a parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Display {
    /// A switch: 0 or 1.
    Boolean,
    /// Whole numbers that have names, listed by
    /// `kAudioUnitProperty_ParameterValueStrings`.
    Indexed,
    /// A number in a unit.
    Number(u32),
}

/// Whether a step's text is a name rather than its own number.
fn is_name(value: f64, label: &str) -> bool {
    label.trim().parse::<f64>() != Ok(value)
}

fn display(param: &dyn Param) -> Display {
    let (min, max) = bounds(param);
    let whole = param.step_count().filter(|&steps| {
        steps > 0 && min.fract() == 0.0 && max.fract() == 0.0 && max - min == f64::from(steps)
    });
    if let Some(steps) = whole {
        // A switch is a boolean, which a host shows as a check box: one
        // that says it is the bypass, or whose two values are named Off
        // and On. Two named choices are a menu.
        let switch = steps == 1 && min == 0.0 && {
            let names = [param.plain_to_text(0.0), param.plain_to_text(1.0)];
            param.info().flags().bypass
                || (names[0].eq_ignore_ascii_case("off") && names[1].eq_ignore_ascii_case("on"))
        };
        if switch {
            return Display::Boolean;
        }
        let named = steps <= 1_024
            && (0..=steps).any(|step| {
                let value = min + f64::from(step);
                is_name(value, &param.plain_to_text(value))
            });
        if named {
            return Display::Indexed;
        }
    }
    let unit = match param.info().unit() {
        "dB" | "db" | "dBFS" => kAudioUnitParameterUnit_Decibels,
        "Hz" | "hz" => kAudioUnitParameterUnit_Hertz,
        "ms" => kAudioUnitParameterUnit_Milliseconds,
        "s" | "sec" => kAudioUnitParameterUnit_Seconds,
        "%" => kAudioUnitParameterUnit_Percent,
        "st" | "semitones" => kAudioUnitParameterUnit_RelativeSemiTones,
        "ct" | "cent" | "cents" => kAudioUnitParameterUnit_Cents,
        "bpm" | "BPM" => kAudioUnitParameterUnit_BPM,
        "°" | "deg" => kAudioUnitParameterUnit_Degrees,
        "oct" | "octaves" => kAudioUnitParameterUnit_Octaves,
        "beat" | "beats" => kAudioUnitParameterUnit_Beats,
        _ => kAudioUnitParameterUnit_Generic,
    };
    Display::Number(unit)
}

/// Whether a parameter moves along a logarithmic scale between its ends.
fn logarithmic(param: &dyn Param) -> bool {
    let (min, max) = bounds(param);
    min > 0.0 && max > min && {
        let at = |fraction: f64| min * (max / min).powf(fraction);
        (param.plain_to_normalized(at(0.5)) - 0.5).abs() < 1e-4
            && (param.plain_to_normalized(at(0.25)) - 0.25).abs() < 1e-4
    }
}

/// Copies `text` into a fixed field of C characters, ending it with a NUL
/// and cutting it at a character boundary if it does not fit.
fn copy_name(text: &str, out: &mut [c_char]) {
    let Some(room) = out.len().checked_sub(1) else {
        return;
    };
    let mut length = text.len().min(room);
    while !text.is_char_boundary(length) {
        length -= 1;
    }
    for (slot, &byte) in out.iter_mut().zip(&text.as_bytes()[..length]) {
        *slot = byte as c_char;
    }
    out[length] = 0;
}

impl<P: Plugin> Instance<P> {
    fn param(&self, id: AudioUnitParameterID) -> Option<&dyn Param> {
        self.params.find_param(ParamId(id))
    }

    fn param_info(&self, param: &dyn Param) -> AudioUnitParameterInfo {
        let (min, max) = bounds(param);
        let mut info = AudioUnitParameterInfo {
            name: [0; 52],
            unitName: ptr::null(),
            clumpID: 0,
            cfNameString: ptr::null(),
            unit: kAudioUnitParameterUnit_Generic,
            minValue: min as f32,
            maxValue: max as f32,
            defaultValue: param.default_plain().clamp(min, max) as f32,
            flags: kAudioUnitParameterFlag_IsReadable | kAudioUnitParameterFlag_IsWritable,
        };
        copy_name(param.name(), &mut info.name);
        if let Some(name) = cf::string(param.name()) {
            // The host releases it, as the flags say.
            info.cfNameString = name.into_raw();
            info.flags |=
                kAudioUnitParameterFlag_HasCFNameString | kAudioUnitParameterFlag_CFNameRelease;
        }
        match display(param) {
            Display::Boolean => info.unit = kAudioUnitParameterUnit_Boolean,
            Display::Indexed => {
                info.unit = kAudioUnitParameterUnit_Indexed;
                info.flags |= kAudioUnitParameterFlag_ValuesHaveStrings;
            }
            Display::Number(unit) => {
                info.unit = unit;
                info.flags |= kAudioUnitParameterFlag_ValuesHaveStrings;
                if logarithmic(param) {
                    info.flags |= kAudioUnitParameterFlag_DisplayLogarithmic;
                }
            }
        }
        info
    }
}

unsafe extern "C" fn get_parameter<P: Plugin>(
    this: *mut c_void,
    id: AudioUnitParameterID,
    scope: AudioUnitScope,
    _element: AudioUnitElement,
    out: *mut AudioUnitParameterValue,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: the host passes the pointer the factory returned.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        if scope != kAudioUnitScope_Global {
            return kAudioUnitErr_InvalidScope;
        }
        let Some(param) = this.param(id) else {
            return kAudioUnitErr_InvalidParameter;
        };
        if out.is_null() {
            return paramErr;
        }
        // SAFETY: the host passes room for a value.
        unsafe { out.write_unaligned(param.plain() as f32) };
        noErr
    })
}

unsafe extern "C" fn set_parameter<P: Plugin>(
    this: *mut c_void,
    id: AudioUnitParameterID,
    scope: AudioUnitScope,
    _element: AudioUnitElement,
    value: AudioUnitParameterValue,
    _offset: u32,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        if scope != kAudioUnitScope_Global {
            return kAudioUnitErr_InvalidScope;
        }
        let Some(param) = this.param(id) else {
            return kAudioUnitErr_InvalidParameter;
        };
        if !value.is_finite() {
            return kAudioUnitErr_InvalidPropertyValue;
        }
        // The plugin hears it on the first frame of the next render.
        param.set_plain(f64::from(value));
        noErr
    })
}

/// Adds the changes a host scheduled for the next render, on their frames.
/// A ramp becomes points every 32 frames and one at its end, within the
/// longest slice; the host schedules what is left of a ramp again for the
/// next one.
unsafe extern "C" fn schedule_parameters<P: Plugin>(
    this: *mut c_void,
    events: *const AudioUnitParameterEvent,
    count: u32,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        if events.is_null() && count > 0 {
            return paramErr;
        }
        let Some(mut audio) = this.audio.try_take() else {
            this.dropped.fetch_add(u64::from(count), Ordering::Relaxed);
            return noErr;
        };
        let limit = audio
            .prepared
            .as_ref()
            .map_or(DEFAULT_MAX_FRAMES, |prepared| prepared.max_frames);
        let mut lost = 0u64;
        for index in 0..count as usize {
            // SAFETY: the host passes `count` events.
            let event = unsafe { events.add(index).read_unaligned() };
            if event.scope != kAudioUnitScope_Global {
                continue;
            }
            let Some(param) = this.param(event.parameter) else {
                continue;
            };
            let id = param.param_id();
            let mut push = |frame: i64, value: f32| {
                if !(0..i64::from(limit)).contains(&frame) || !value.is_finite() {
                    return;
                }
                let kind = EventKind::ParamValue {
                    param: id,
                    value: f64::from(value),
                };
                if audio
                    .pending
                    .push(Event {
                        offset: frame as u32,
                        kind,
                    })
                    .is_err()
                {
                    lost += 1;
                }
            };
            let words = event.eventValues;
            match event.eventType {
                kParameterEvent_Immediate => {
                    push(i64::from(words[0]), f32::from_bits(words[1]));
                }
                kParameterEvent_Ramped => {
                    let start = i64::from(words[0] as i32);
                    let duration = i64::from(words[1]);
                    let (from, to) = (f32::from_bits(words[2]), f32::from_bits(words[3]));
                    let end = start + duration;
                    let at = |frame: i64| {
                        if duration == 0 {
                            return to;
                        }
                        let fraction = (frame - start) as f64 / duration as f64;
                        (f64::from(from) + (f64::from(to) - f64::from(from)) * fraction) as f32
                    };
                    let mut frame = start.max(0);
                    while frame < end.min(i64::from(limit)) {
                        push(frame, at(frame));
                        frame += RAMP_STEP;
                    }
                    push(end, to);
                }
                _ => {}
            }
        }
        this.dropped.fetch_add(lost, Ordering::Relaxed);
        noErr
    })
}

// --- MIDI ---------------------------------------------------------------------------

unsafe extern "C" fn midi_event<P: Plugin>(
    this: *mut c_void,
    status: u32,
    data1: u32,
    data2: u32,
    offset: u32,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        let status = status as u8;
        let length = match status & 0xF0 {
            0xC0 | 0xD0 => 2,
            0x80..=0xE0 => 3,
            // System messages have no place in a plugin's event stream.
            _ => return noErr,
        };
        let bytes = [status, (data1 & 0x7F) as u8, (data2 & 0x7F) as u8];
        let Some(message) = MidiMessage::from_bytes(&bytes[..length]) else {
            return noErr;
        };
        let Some(mut audio) = this.audio.try_take() else {
            this.dropped.fetch_add(1, Ordering::Relaxed);
            return noErr;
        };
        if audio
            .pending
            .push(Event::from_midi(offset, message))
            .is_err()
        {
            this.dropped.fetch_add(1, Ordering::Relaxed);
        }
        noErr
    })
}

/// `MusicDeviceStartNote`: a note on the channel of the group (its low four
/// bits), at the pitch rounded to a key, as a MIDI note-on on its frame.
/// The note's instance id is its channel and key, which `StopNote` takes.
unsafe extern "C" fn start_note<P: Plugin>(
    this: *mut c_void,
    _instrument: u32,
    group: u32,
    note: *mut u32,
    offset: u32,
    params: *const MusicDeviceNoteParams,
) -> OSStatus {
    guard(paramErr, || {
        if params.is_null() {
            return paramErr;
        }
        // SAFETY: the host passes the note's parameters.
        let params = unsafe { params.read_unaligned() };
        if params.argCount < 2 || !params.mPitch.is_finite() || !params.mVelocity.is_finite() {
            return paramErr;
        }
        let key = params.mPitch.round().clamp(0.0, 127.0) as u32;
        let velocity = params.mVelocity.round().clamp(1.0, 127.0) as u32;
        let channel = group & 0x0F;
        if !note.is_null() {
            // SAFETY: the host passes a place for the note's id.
            unsafe { note.write_unaligned((channel << 8) | key) };
        }
        // SAFETY: the same instance, with a note-on.
        unsafe { midi_event::<P>(this, 0x90 | channel, key, velocity, offset) }
    })
}

/// `MusicDeviceStopNote`: the note `StartNote` gave this id, ended as a
/// MIDI note-off on its frame.
unsafe extern "C" fn stop_note<P: Plugin>(
    this: *mut c_void,
    _group: u32,
    note: u32,
    offset: u32,
) -> OSStatus {
    let (channel, key) = ((note >> 8) & 0x0F, note & 0x7F);
    // SAFETY: the same instance, with a note-off.
    unsafe { midi_event::<P>(this, 0x80 | channel, key, 0, offset) }
}

unsafe extern "C" fn sysex(_this: *mut c_void, _data: *const u8, _length: u32) -> OSStatus {
    // System exclusive messages have no place in the event stream either.
    noErr
}

// --- Rendering ----------------------------------------------------------------------

/// The `AudioBuffer`s of a list, as a slice.
///
/// # Safety
///
/// `list` must point to an `AudioBufferList` that holds as many buffers as
/// its count says, readable and writable for as long as the slice lives.
unsafe fn buffers<'a>(list: *mut AudioBufferList) -> &'a mut [AuBuffer] {
    // SAFETY: per this function's contract.
    unsafe {
        let count = (*list).mNumberBuffers as usize;
        let first = list
            .cast::<u8>()
            .add(AUDIO_BUFFER_LIST_BUFFERS)
            .cast::<AuBuffer>();
        std::slice::from_raw_parts_mut(first, count)
    }
}

/// Silences what the host gave as the output, and says so.
///
/// # Safety
///
/// `list` must be null or as for [`buffers`], each buffer with `mData` null
/// or holding `mDataByteSize` writable bytes.
unsafe fn silence(list: *mut AudioBufferList, flags: *mut AudioUnitRenderActionFlags) {
    if list.is_null() {
        return;
    }
    // SAFETY: per this function's contract.
    for buffer in unsafe { buffers(list) } {
        if !buffer.mData.is_null() {
            // SAFETY: as above.
            unsafe {
                ptr::write_bytes(buffer.mData.cast::<u8>(), 0, buffer.mDataByteSize as usize)
            };
        }
    }
    if !flags.is_null() {
        // SAFETY: the host passes its flags.
        unsafe { *flags |= kAudioUnitRenderAction_OutputIsSilence };
    }
}

impl Pull {
    /// Has `source` render `frames` frames into this element's memory, and
    /// copies them into `into`. Returns the source's status.
    ///
    /// # Safety
    ///
    /// The source must be the host's, as it set it up.
    unsafe fn fetch(
        &mut self,
        source: Source,
        timestamp: *const AudioTimeStamp,
        element: u32,
        frames: usize,
        into: &mut AudioBuffer<f32>,
    ) -> OSStatus {
        let channels = self.storage.num_channels();
        self.storage.set_num_frames(frames);
        let list = self.list.as_mut_ptr().cast::<AudioBufferList>();
        // SAFETY: the list's words hold the header and `channels` buffers.
        unsafe {
            (*list).mNumberBuffers = channels as u32;
            for (buffer, samples) in buffers(list)
                .iter_mut()
                .zip(self.storage.iter_channels_mut())
            {
                *buffer = AuBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: (frames * 4) as u32,
                    mData: samples.as_mut_ptr().cast(),
                };
            }
        }
        let mut flags: AudioUnitRenderActionFlags = 0;
        let status = match source {
            Source::None => kAudioUnitErr_NoConnection,
            Source::Callback(callback) => match callback.inputProc {
                // SAFETY: the host's own callback and data, with a list of
                // buffers of `frames` frames this value owns.
                Some(call) => unsafe {
                    call(
                        callback.inputProcRefCon,
                        &mut flags,
                        timestamp,
                        element,
                        frames as u32,
                        list,
                    )
                },
                None => kAudioUnitErr_NoConnection,
            },
            // SAFETY: the unit the host connected, rendered as AudioToolbox
            // renders any unit.
            Source::Connection(connection) => unsafe {
                AudioUnitRender(
                    connection.sourceAudioUnit,
                    &mut flags,
                    timestamp,
                    connection.sourceOutputNumber,
                    frames as u32,
                    list,
                )
            },
        };
        into.clear();
        if status != noErr {
            return status;
        }
        // The source may have pointed the buffers at its own memory.
        // SAFETY: the list is as the source left it, with each buffer's
        // memory valid for this render.
        let given = unsafe { buffers(list) };
        for (channel, buffer) in given.iter().enumerate().take(into.num_channels()) {
            let available = (buffer.mDataByteSize as usize / 4).min(frames);
            if buffer.mData.is_null() || available == 0 {
                continue;
            }
            // SAFETY: the buffer holds `mDataByteSize` bytes of samples.
            let samples =
                unsafe { std::slice::from_raw_parts(buffer.mData.cast::<f32>(), available) };
            into.channel_mut(channel)[..available].copy_from_slice(samples);
        }
        noErr
    }
}

/// The transport, as the host's callbacks give it.
///
/// # Safety
///
/// The callbacks must be the host's, as it gave them.
unsafe fn transport(host: Option<&HostCallbackInfo>) -> Transport {
    let mut transport = Transport::STOPPED;
    let Some(host) = host else {
        return transport;
    };
    let user = host.hostUserData;
    let finite = |value: f64| value.is_finite().then_some(value);
    if let Some(call) = host.beatAndTempoProc {
        let (mut beat, mut tempo) = (f64::NAN, f64::NAN);
        // SAFETY: the host's callback, with places for its answers.
        if unsafe { call(user, &mut beat, &mut tempo) } == noErr {
            transport.position_beats = finite(beat);
            transport.tempo_bpm = finite(tempo).filter(|&tempo| tempo > 0.0);
        }
    }
    if let Some(call) = host.musicalTimeLocationProc {
        let (mut next, mut numerator, mut denominator, mut downbeat) = (0u32, 0f32, 0u32, f64::NAN);
        // SAFETY: as above.
        if unsafe {
            call(
                user,
                &mut next,
                &mut numerator,
                &mut denominator,
                &mut downbeat,
            )
        } == noErr
        {
            if numerator >= 1.0
                && numerator <= f32::from(u16::MAX)
                && (1..=65_535).contains(&denominator)
            {
                transport.time_signature = Some((numerator.round() as u16, denominator as u16));
            }
            transport.bar_start_beats = finite(downbeat);
        }
    }
    let (mut playing, mut recording, mut changed, mut cycling) = (0u8, 0u8, 0u8, 0u8);
    let (mut sample, mut cycle_start, mut cycle_end) = (f64::NAN, f64::NAN, f64::NAN);
    // SAFETY: as above.
    let state = unsafe {
        match (host.transportStateProc2, host.transportStateProc) {
            (Some(call), _) => Some(call(
                user,
                &mut playing,
                &mut recording,
                &mut changed,
                &mut sample,
                &mut cycling,
                &mut cycle_start,
                &mut cycle_end,
            )),
            (None, Some(call)) => Some(call(
                user,
                &mut playing,
                &mut changed,
                &mut sample,
                &mut cycling,
                &mut cycle_start,
                &mut cycle_end,
            )),
            (None, None) => None,
        }
    };
    if state == Some(noErr) {
        transport.playing = playing != 0;
        transport.recording = recording != 0;
        transport.position_samples = finite(sample)
            .filter(|sample| sample.abs() < 9.0e18)
            .map(|sample| sample.floor() as i64);
        transport.loop_beats = match (cycling != 0, finite(cycle_start), finite(cycle_end)) {
            (true, Some(start), Some(end)) if start < end => Some((start, end)),
            _ => None,
        };
    }
    transport
}

impl<P: Plugin> Instance<P> {
    /// Calls the render notifications, if no other thread is changing them.
    ///
    /// # Safety
    ///
    /// The arguments must be the host's own, from its `Render` call.
    unsafe fn notify_render(
        &self,
        flag: AudioUnitRenderActionFlags,
        flags: *mut AudioUnitRenderActionFlags,
        timestamp: *const AudioTimeStamp,
        frames: u32,
        list: *mut AudioBufferList,
    ) {
        // Copied out, so that a notification may add or remove one.
        let mut copied = [Notification {
            proc_: None,
            user: ptr::null_mut(),
        }; MAX_RENDER_NOTIFICATIONS];
        let count = {
            let Some(notifications) = self.notifications.try_take() else {
                return;
            };
            for (slot, notification) in copied.iter_mut().zip(notifications.iter()) {
                *slot = *notification;
            }
            notifications.len().min(MAX_RENDER_NOTIFICATIONS)
        };
        for notification in &copied[..count] {
            let Some(call) = notification.proc_ else {
                continue;
            };
            let mut these = if flags.is_null() {
                flag
            } else {
                // SAFETY: the host's flags.
                unsafe { *flags | flag }
            };
            // SAFETY: the host's own notification and data, called as it
            // asked to be, with the arguments of its render.
            unsafe { call(notification.user, &mut these, timestamp, 0, frames, list) };
        }
    }

    /// Renders one slice. Real-time safe.
    ///
    /// # Safety
    ///
    /// The arguments must be the host's own, from its `Render` call.
    unsafe fn render(
        &self,
        flags: *mut AudioUnitRenderActionFlags,
        timestamp: *const AudioTimeStamp,
        bus: u32,
        frames: u32,
        list: *mut AudioBufferList,
    ) -> OSStatus {
        if bus as usize > P::AUX_OUTPUTS.len() {
            return kAudioUnitErr_InvalidElement;
        }
        if list.is_null() {
            return paramErr;
        }
        if !self.initialized.load(Ordering::Acquire) {
            return kAudioUnitErr_Uninitialized;
        }
        // Before any lock is taken, since a notification may call back
        // into the unit.
        // SAFETY: the host's arguments.
        unsafe {
            self.notify_render(
                kAudioUnitRenderAction_PreRender,
                flags,
                timestamp,
                frames,
                list,
            )
        };
        let Some(mut audio) = self.audio.try_take() else {
            // SAFETY: the host's list and flags.
            unsafe { silence(list, flags) };
            return noErr;
        };
        // The configuration is copied out and let go at once: an input's
        // callback is the host's, and may ask the unit for a property on
        // this thread, which would otherwise wait for itself.
        let config = {
            let Some(config) = self.config.try_take() else {
                // SAFETY: the host's list and flags.
                unsafe { silence(list, flags) };
                return noErr;
            };
            Rendering {
                sources: config.sources,
                host: config.host,
                bypass: config.bypass,
                midi_out: config.midi_out,
            }
        };
        let Audio {
            plugin,
            prepared,
            pending,
            events,
            watcher,
            output,
            translator,
            packets,
        } = &mut *audio;
        let Some(prepared) = prepared.as_mut() else {
            return kAudioUnitErr_Uninitialized;
        };
        if frames > prepared.max_frames {
            return kAudioUnitErr_TooManyFramesToProcess;
        }
        // A host pulls each output element of a unit with several for the
        // same slice, in any order: the plugin runs for the first, and the
        // others are read from what it wrote then, as Apple's own base
        // classes do. A unit with one output runs for every render.
        // SAFETY: the host's time stamp, or null.
        let stamp = unsafe { timestamp.as_ref() }
            .filter(|stamp| stamp.mFlags & kAudioTimeStampSampleTimeValid != 0)
            .map(|stamp| (stamp.mSampleTime, frames))
            .filter(|_| !P::AUX_OUTPUTS.is_empty());
        let cached = stamp.is_some() && prepared.rendered == stamp;
        let frames = frames as usize;
        if !cached {
            prepared.rendered = stamp;
            let status = self.run_plugin(
                plugin, prepared, pending, events, watcher, output, &config, timestamp, frames,
            );
            if status != noErr {
                return status;
            }
        }

        // The output: into the host's memory, or, where it gave none, in
        // the unit's own, which stays valid until the next render.
        let source = match bus {
            0 => &mut prepared.buffer,
            aux => match prepared.aux.bus_mut(aux as usize - 1) {
                Some(written) => written,
                None => return kAudioUnitErr_InvalidElement,
            },
        };
        // SAFETY: the host's list, with as many buffers as it says.
        for (channel, buffer) in unsafe { buffers(list) }.iter_mut().enumerate() {
            if buffer.mNumberChannels > 1 {
                return kAudioUnitErr_FormatNotSupported;
            }
            let ours = (channel < source.num_channels()).then(|| source.channel_mut(channel));
            if buffer.mData.is_null() {
                match ours {
                    Some(samples) => {
                        buffer.mData = samples.as_mut_ptr().cast();
                        buffer.mDataByteSize = (frames * 4) as u32;
                        buffer.mNumberChannels = 1;
                    }
                    None => return kAudioUnitErr_FormatNotSupported,
                }
                continue;
            }
            let room = (buffer.mDataByteSize as usize / 4).min(frames);
            // SAFETY: the host's buffer holds `mDataByteSize` bytes.
            let target =
                unsafe { std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), room) };
            match ours {
                Some(samples) => target.copy_from_slice(&samples[..room]),
                None => target.fill(0.0),
            }
            buffer.mDataByteSize = (room * 4) as u32;
        }
        if let Some(midi_out) = config.midi_out.filter(|_| !cached && !output.is_empty()) {
            // SAFETY: the host's callback and data, on its render thread,
            // with its own time stamp.
            unsafe { send_midi(output, translator, packets, midi_out, timestamp, frames) };
        }
        drop(audio);
        // SAFETY: the host's arguments.
        unsafe {
            self.notify_render(
                kAudioUnitRenderAction_PostRender,
                flags,
                timestamp,
                frames as u32,
                list,
            )
        };
        noErr
    }

    /// Pulls the input elements and runs the plugin for one slice of
    /// `frames` frames, into the unit's own memory. Real-time safe.
    #[allow(clippy::too_many_arguments)]
    fn run_plugin(
        &self,
        plugin: &mut P,
        prepared: &mut Prepared,
        pending: &mut EventBuffer,
        events: &mut EventBuffer,
        watcher: &mut ParamWatcher,
        output: &mut OutputBuffer,
        config: &Rendering,
        timestamp: *const AudioTimeStamp,
        frames: usize,
    ) -> OSStatus {
        // The input elements: the main input, which must be connected, and
        // the sidechain, which is silent when it is not.
        prepared.buffer.set_num_frames(frames);
        prepared.sidechain.set_num_frames(frames);
        let has_main = P::LAYOUT.inputs > 0;
        for (element, fetch) in prepared.pulls.iter_mut().enumerate() {
            let main = has_main && element == 0;
            let into = if main {
                &mut prepared.buffer
            } else {
                &mut prepared.sidechain
            };
            let source = config.sources[element];
            // SAFETY: the host set the source up, for this element.
            let status = unsafe { fetch.fetch(source, timestamp, element as u32, frames, into) };
            if status != noErr && main {
                self.last_error.store(status, Ordering::Relaxed);
                return status;
            }
        }
        if !has_main {
            prepared.buffer.clear();
        }
        // Channels beyond the plugin's inputs are silent.
        for channel in prepared.layout.inputs..prepared.buffer.num_channels() {
            prepared.buffer.channel_mut(channel).fill(0.0);
        }
        output.clear();

        if !config.bypass {
            let params: &dyn ParamSet = &*self.params;
            events.clear();
            watcher.emit_changes(params, events);
            let announced = events.len();
            let last = frames.saturating_sub(1) as u32;
            for event in pending.iter() {
                let event = Event {
                    offset: event.offset.min(last),
                    ..*event
                };
                if events.push(event).is_err() {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            pending.clear();
            watcher.apply_and_remember(params, &events.as_slice()[announced..]);
            // SAFETY: the host's callbacks, on its render thread.
            let transport = unsafe { transport(config.host.as_ref()) };
            let mut context = ProcessContext::new(events, &transport);
            if prepared.layout.sidechain > 0 {
                context = context.with_sidechain(&prepared.sidechain);
            }
            prepared.aux.start_block(frames);
            plugin.process_with_aux(&mut prepared.buffer, &context, output, &mut prepared.aux);
        } else {
            pending.clear();
            // Bypassed, the auxiliary outputs are silent.
            prepared.aux.start_block(frames);
        }
        let latency = u32::try_from(plugin.latency_samples()).unwrap_or(u32::MAX);
        self.latency.store(latency, Ordering::Relaxed);
        noErr
    }
}

/// Writes MIDI into a `MIDIPacketList`, one message to a packet, laid out
/// as CoreMIDI's headers declare it (see [`MIDI_PACKET_LIST_PACKETS`]).
struct PacketList<'a> {
    bytes: &'a mut [u8],
    count: u32,
    /// Where the next packet starts.
    end: usize,
}

impl<'a> PacketList<'a> {
    fn new(words: &'a mut [u32]) -> Self {
        let len = words.len() * 4;
        // SAFETY: the words are `len` bytes of plain memory, borrowed
        // mutably for as long as the bytes are.
        let bytes = unsafe { std::slice::from_raw_parts_mut(words.as_mut_ptr().cast::<u8>(), len) };
        Self {
            bytes,
            count: 0,
            end: MIDI_PACKET_LIST_PACKETS,
        }
    }

    /// Adds a packet of `data` at `time`. Returns whether it fitted.
    fn push(&mut self, time: u64, data: &[u8]) -> bool {
        let start = self.end;
        let end = start + MIDI_PACKET_DATA + data.len();
        if end > self.bytes.len() {
            return false;
        }
        self.bytes[start..start + 8].copy_from_slice(&time.to_ne_bytes());
        self.bytes[start + MIDI_PACKET_LENGTH..start + MIDI_PACKET_DATA]
            .copy_from_slice(&(data.len() as u16).to_ne_bytes());
        self.bytes[start + MIDI_PACKET_DATA..end].copy_from_slice(data);
        self.end = end.next_multiple_of(MIDI_PACKET_ALIGN);
        self.count += 1;
        true
    }

    /// The list, its count written, for the host.
    fn finish(&mut self) -> *const c_void {
        self.bytes[..4].copy_from_slice(&self.count.to_ne_bytes());
        self.bytes.as_ptr().cast()
    }

    fn clear(&mut self) {
        self.count = 0;
        self.end = MIDI_PACKET_LIST_PACKETS;
    }
}

/// Hands what the plugin sent in a render of `frames` frames to the host's
/// MIDI output callback, as MIDI 1.0 by the default translation, each
/// packet stamped with the frame of the render its message falls on, as
/// the time stamps of `kAudioUnitProperty_MIDIOutputCallback` are counted.
/// A render that sends more than one list holds is handed over in several,
/// in order. Real-time safe.
///
/// # Safety
///
/// `callback` and `timestamp` must be the host's, valid on this thread.
unsafe fn send_midi(
    output: &OutputBuffer,
    translator: &mut OutputTranslator,
    words: &mut [u32],
    callback: AUMIDIOutputCallbackStruct,
    timestamp: *const AudioTimeStamp,
    frames: usize,
) {
    let Some(call) = callback.midiOutputCallback else {
        return;
    };
    let mut list = PacketList::new(words);
    let hand_over = |list: &mut PacketList<'_>| {
        if list.count > 0 {
            // SAFETY: the host's callback and data, with a list it reads
            // during the call, per this function's contract.
            unsafe { call(callback.userData, timestamp, 0, list.finish()) };
            list.clear();
        }
    };
    let last = frames.saturating_sub(1) as u32;
    for event in output {
        let time = u64::from(event.offset.min(last));
        translator.translate(&event.kind, |packet| {
            let Some(message) = packet.to_midi1() else {
                return;
            };
            let bytes = message.to_bytes();
            if !list.push(time, bytes.as_slice()) {
                hand_over(&mut list);
                let _ = list.push(time, bytes.as_slice());
            }
        });
    }
    hand_over(&mut list);
}

unsafe extern "C" fn render<P: Plugin>(
    this: *mut c_void,
    flags: *mut AudioUnitRenderActionFlags,
    timestamp: *const AudioTimeStamp,
    bus: u32,
    frames: u32,
    list: *mut AudioBufferList,
) -> OSStatus {
    // Numbers too small to hear are zero while the plugin runs, and the
    // host's own setting is back when this returns.
    let _quiet = NoDenormals::new();
    // SAFETY: the host passes the pointer the factory returned.
    let Some(this) = (unsafe { instance::<P>(this) }) else {
        return paramErr;
    };
    let status = guard(None, || {
        // SAFETY: the host's own arguments.
        Some(unsafe { this.render(flags, timestamp, bus, frames, list) })
    });
    match status {
        Some(status) => status,
        None => {
            // The plugin panicked: the host hears silence.
            // SAFETY: the host's list and flags.
            unsafe { silence(list, flags) };
            this.last_error
                .store(kAudioUnitErr_CannotDoInCurrentContext, Ordering::Relaxed);
            kAudioUnitErr_CannotDoInCurrentContext
        }
    }
}

// --- Listeners and notifications ---------------------------------------------------

unsafe extern "C" fn add_property_listener<P: Plugin>(
    this: *mut c_void,
    property: AudioUnitPropertyID,
    proc_: AudioUnitPropertyListenerProc,
    user: *mut c_void,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: the host passes the pointer the factory returned.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        if proc_.is_none() {
            return paramErr;
        }
        let mut listeners = lock(&this.listeners);
        if listeners.len() >= MAX_LISTENERS {
            return kAudioUnitErr_CannotDoInCurrentContext;
        }
        listeners.push(Listener {
            property,
            proc_,
            user,
        });
        noErr
    })
}

/// Whether two listeners are the same function, compared by address as
/// the host knows them.
fn same_proc(a: AudioUnitPropertyListenerProc, b: AudioUnitPropertyListenerProc) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => std::ptr::fn_addr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

unsafe extern "C" fn remove_property_listener<P: Plugin>(
    this: *mut c_void,
    property: AudioUnitPropertyID,
    proc_: AudioUnitPropertyListenerProc,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        lock(&this.listeners).retain(|listener| {
            !(listener.property == property && same_proc(listener.proc_, proc_))
        });
        noErr
    })
}

unsafe extern "C" fn remove_property_listener_with_user_data<P: Plugin>(
    this: *mut c_void,
    property: AudioUnitPropertyID,
    proc_: AudioUnitPropertyListenerProc,
    user: *mut c_void,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        lock(&this.listeners).retain(|listener| {
            !(listener.property == property
                && same_proc(listener.proc_, proc_)
                && listener.user == user)
        });
        noErr
    })
}

unsafe extern "C" fn add_render_notify<P: Plugin>(
    this: *mut c_void,
    proc_: AURenderCallback,
    user: *mut c_void,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        if proc_.is_none() {
            return paramErr;
        }
        let mut notifications = this.notifications.take();
        let notification = Notification { proc_, user };
        if !notifications.iter().any(|known| known.is(proc_, user)) {
            if notifications.len() >= MAX_RENDER_NOTIFICATIONS {
                return kAudioUnitErr_CannotDoInCurrentContext;
            }
            notifications.push(notification);
        }
        noErr
    })
}

unsafe extern "C" fn remove_render_notify<P: Plugin>(
    this: *mut c_void,
    proc_: AURenderCallback,
    user: *mut c_void,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        this.notifications
            .take()
            .retain(|notification| !notification.is(proc_, user));
        noErr
    })
}

// --- Properties ------------------------------------------------------------------

/// The stream format of `channels` channels at `sample_rate`: 32-bit float,
/// one buffer per channel, as Audio Units exchange audio.
fn stream_format(sample_rate: f64, channels: usize) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: sample_rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat
            | kAudioFormatFlagIsPacked
            | kAudioFormatFlagIsNonInterleaved,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: channels as u32,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// Whether a format the host asks for is one the unit takes for an element
/// of `channels` channels.
fn acceptable(format: &AudioStreamBasicDescription, channels: usize) -> bool {
    let flags = format.mFormatFlags;
    format.mSampleRate.is_finite()
        && format.mSampleRate > 0.0
        && format.mFormatID == kAudioFormatLinearPCM
        && flags & kAudioFormatFlagIsFloat != 0
        && flags & kAudioFormatFlagIsNonInterleaved != 0
        && flags & kAudioFormatFlagIsBigEndian
            == u32::from(cfg!(target_endian = "big")) * kAudioFormatFlagIsBigEndian
        && format.mBitsPerChannel == 32
        && format.mChannelsPerFrame as usize == channels
}

/// The channel layout for `channels` channels.
fn layout_tag(channels: usize) -> AudioChannelLayoutTag {
    match channels {
        1 => kAudioChannelLayoutTag_Mono,
        2 => kAudioChannelLayoutTag_Stereo,
        other => kAudioChannelLayoutTag_DiscreteInOrder | (other as u32 & 0xFFFF),
    }
}

/// What a property is, for `GetPropertyInfo`: its size and whether it can
/// be set.
impl<P: Plugin> Instance<P> {
    fn describe(
        &self,
        property: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
    ) -> Result<(usize, bool), OSStatus> {
        let initialized = self.initialized.load(Ordering::Acquire);
        let global = |size: usize, writable: bool| {
            if scope == kAudioUnitScope_Global {
                Ok((size, writable))
            } else {
                Err(kAudioUnitErr_InvalidScope)
            }
        };
        let pointer = size_of::<*const c_void>();
        match property {
            kAudioUnitProperty_ClassInfo => global(pointer, true),
            kAudioUnitProperty_PresentPreset | kAudioUnitProperty_CurrentPreset => {
                global(size_of::<AUPreset>(), true)
            }
            kAudioUnitProperty_SampleRate => {
                self.channels(scope, element)?;
                Ok((size_of::<f64>(), !initialized))
            }
            kAudioUnitProperty_StreamFormat => {
                self.channels(scope, element)?;
                Ok((size_of::<AudioStreamBasicDescription>(), !initialized))
            }
            kAudioUnitProperty_ParameterList => match scope {
                kAudioUnitScope_Global => Ok((self.ids.len() * 4, false)),
                kAudioUnitScope_Input | kAudioUnitScope_Output => Ok((0, false)),
                _ => Err(kAudioUnitErr_InvalidScope),
            },
            kAudioUnitProperty_ParameterInfo => {
                if scope != kAudioUnitScope_Global {
                    return Err(kAudioUnitErr_InvalidScope);
                }
                self.param(element).ok_or(kAudioUnitErr_InvalidParameter)?;
                Ok((size_of::<AudioUnitParameterInfo>(), false))
            }
            kAudioUnitProperty_ParameterValueStrings => {
                if scope != kAudioUnitScope_Global {
                    return Err(kAudioUnitErr_InvalidScope);
                }
                let param = self.param(element).ok_or(kAudioUnitErr_InvalidParameter)?;
                if display(param) != Display::Indexed {
                    return Err(kAudioUnitErr_InvalidProperty);
                }
                Ok((pointer, false))
            }
            kAudioUnitProperty_ParameterStringFromValue => {
                global(size_of::<AudioUnitParameterStringFromValue>(), false)
            }
            kAudioUnitProperty_ParameterValueFromString => {
                global(size_of::<AudioUnitParameterValueFromString>(), false)
            }
            kAudioUnitProperty_ElementCount => match scope {
                kAudioUnitScope_Global | kAudioUnitScope_Output => Ok((4, false)),
                kAudioUnitScope_Input => Ok((4, false)),
                _ => Err(kAudioUnitErr_InvalidScope),
            },
            kAudioUnitProperty_Latency | kAudioUnitProperty_TailTime => global(8, false),
            kAudioUnitProperty_SupportedNumChannels => global(
                self.channel_infos().len() * size_of::<AUChannelInfo>(),
                false,
            ),
            kAudioUnitProperty_MIDIOutputCallbackInfo if P::NOTE_OUTPUT => global(pointer, false),
            kAudioUnitProperty_MIDIOutputCallback if P::NOTE_OUTPUT => {
                global(size_of::<AUMIDIOutputCallbackStruct>(), true)
            }
            // Writable at any time, as in Apple's base classes: an
            // initialised unit is prepared again for the new length.
            kAudioUnitProperty_MaximumFramesPerSlice => global(4, true),
            kAudioUnitProperty_LastRenderError => global(4, false),
            kAudioUnitProperty_HostCallbacks => global(size_of::<HostCallbackInfo>(), true),
            kAudioUnitProperty_InPlaceProcessing => global(4, true),
            kAudioUnitProperty_BypassEffect if P::LAYOUT.inputs > 0 => global(4, true),
            kAudioUnitProperty_SetRenderCallback => {
                if scope != kAudioUnitScope_Input {
                    return Err(kAudioUnitErr_InvalidScope);
                }
                self.channels(scope, element)?;
                Ok((size_of::<AURenderCallbackStruct>(), true))
            }
            kAudioUnitProperty_MakeConnection => {
                if scope != kAudioUnitScope_Input {
                    return Err(kAudioUnitErr_InvalidScope);
                }
                self.channels(scope, element)?;
                Ok((size_of::<AudioUnitConnection>(), true))
            }
            kAudioUnitProperty_ElementName => {
                self.channels(scope, element)?;
                Ok((pointer, false))
            }
            kAudioUnitProperty_SupportedChannelLayoutTags => {
                self.channels(scope, element)?;
                let count = self.widths(scope, element).len();
                Ok((count * size_of::<AudioChannelLayoutTag>(), false))
            }
            kAudioUnitProperty_AudioChannelLayout => {
                self.channels(scope, element)?;
                Ok((CHANNEL_LAYOUT_DESCRIPTIONS, true))
            }
            kAudioUnitProperty_ShouldAllocateBuffer => {
                self.channels(scope, element)?;
                Ok((4, true))
            }
            _ => Err(kAudioUnitErr_InvalidProperty),
        }
    }
}

/// Writes `value` where the host asked for a property's value.
///
/// # Safety
///
/// `out` must hold `*size` writable bytes.
unsafe fn put<T: Copy>(out: *mut c_void, size: *mut u32, value: T) -> OSStatus {
    // SAFETY: per this function's contract.
    unsafe {
        if (*size as usize) < size_of::<T>() {
            return kAudioUnitErr_InvalidPropertyValue;
        }
        out.cast::<T>().write_unaligned(value);
        *size = size_of::<T>() as u32;
    }
    noErr
}

/// Writes as many of `values` as fit.
///
/// # Safety
///
/// As for [`put`].
unsafe fn put_all<T: Copy>(out: *mut c_void, size: *mut u32, values: &[T]) -> OSStatus {
    // SAFETY: per this function's contract.
    unsafe {
        let fit = (*size as usize / size_of::<T>()).min(values.len());
        for (index, value) in values.iter().take(fit).enumerate() {
            out.cast::<T>().add(index).write_unaligned(*value);
        }
        *size = (fit * size_of::<T>()) as u32;
    }
    noErr
}

/// Hands a new CoreFoundation object to the host, which releases it.
///
/// # Safety
///
/// As for [`put`].
unsafe fn put_object(out: *mut c_void, size: *mut u32, object: Option<Owned>) -> OSStatus {
    let Some(object) = object else {
        return kAudioUnitErr_CannotDoInCurrentContext;
    };
    // SAFETY: per this function's contract.
    unsafe {
        if (*size as usize) < size_of::<CFTypeRef>() {
            return kAudioUnitErr_InvalidPropertyValue;
        }
        out.cast::<CFTypeRef>().write_unaligned(object.into_raw());
        *size = size_of::<CFTypeRef>() as u32;
    }
    noErr
}

/// Reads a property's value of type `T` from the host.
///
/// # Safety
///
/// `data` must be null or hold `size` readable bytes.
unsafe fn take<T: Copy>(data: *const c_void, size: u32) -> Option<T> {
    // SAFETY: per this function's contract.
    (!data.is_null() && size as usize >= size_of::<T>())
        .then(|| unsafe { data.cast::<T>().read_unaligned() })
}

const STATE_KEYS: [&str; 6] = ["version", "type", "subtype", "manufacturer", "name", "data"];

impl<P: Plugin> Instance<P> {
    /// The plugin's state as the dictionary `kAudioUnitProperty_ClassInfo`
    /// holds: the codes, the name and the version of the dictionary's
    /// layout, which Apple's base classes write as 0, and the state's text.
    fn class_info(&self) -> Option<Owned> {
        let dictionary = cf::dictionary()?;
        let text = save_state::<P>(&self.params);
        let values = [
            cf::number(0)?,
            cf::number(self.codes[0] as i32)?,
            cf::number(self.codes[1] as i32)?,
            cf::number(self.codes[2] as i32)?,
            cf::string(&lock(&self.preset).text)?,
            cf::data(text.as_bytes())?,
        ];
        for (key, value) in STATE_KEYS.iter().zip(&values) {
            if !cf::set(&dictionary, key, value) {
                return None;
            }
        }
        Some(dictionary)
    }

    /// Restores a dictionary [`class_info`](Self::class_info) made, or one
    /// as Apple's base classes write it. Another unit's state is refused,
    /// and nothing is changed.
    ///
    /// # Safety
    ///
    /// `list` must be null or a live CoreFoundation object.
    unsafe fn restore(&self, list: CFPropertyListRef) -> OSStatus {
        // SAFETY: per this function's contract.
        unsafe {
            if !cf::is(list, CFDictionaryGetTypeID()) {
                return kAudioUnitErr_InvalidPropertyValue;
            }
            for (key, code) in STATE_KEYS[1..4].iter().zip(self.codes) {
                let value = cf::get(list, key);
                if !value.is_null() && cf::read_number(value).map(|n| n as u32) != Some(code) {
                    return kAudioUnitErr_InvalidPropertyValue;
                }
            }
            let Some(bytes) = cf::read_data(cf::get(list, "data"), MAX_STATE_BYTES) else {
                return kAudioUnitErr_InvalidPropertyValue;
            };
            let Ok(text) = std::str::from_utf8(bytes) else {
                return kAudioUnitErr_InvalidPropertyValue;
            };
            if load_state::<P>(&self.params, text).is_err() {
                return kAudioUnitErr_InvalidPropertyValue;
            }
            // The state's name is the present preset's, as a user's preset.
            if let Some(name) = cf::read_string(cf::get(list, "name")) {
                *lock(&self.preset) = Preset::named(-1, &name);
            }
            noErr
        }
    }

    /// Answers `GetProperty`.
    ///
    /// # Safety
    ///
    /// `out` must hold `*size` writable bytes.
    unsafe fn get(
        &self,
        property: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        out: *mut c_void,
        size: *mut u32,
    ) -> OSStatus {
        let (needed, _) = match self.describe(property, scope, element) {
            Ok(known) => known,
            Err(status) => return status,
        };
        if out.is_null() {
            // The host asks how large the value is.
            // SAFETY: the host passes its size.
            unsafe { *size = needed as u32 };
            return noErr;
        }
        let config = || self.config.take();
        // SAFETY: per this function's contract, for each `put`.
        unsafe {
            match property {
                kAudioUnitProperty_ClassInfo => put_object(out, size, self.class_info()),
                kAudioUnitProperty_PresentPreset | kAudioUnitProperty_CurrentPreset => {
                    if (*size as usize) < size_of::<AUPreset>() {
                        return kAudioUnitErr_InvalidPropertyValue;
                    }
                    // The present preset's name follows the Get rule: the
                    // caller releases it. The deprecated property's does
                    // not, and stays the unit's.
                    let retained = property == kAudioUnitProperty_PresentPreset;
                    put(out, size, lock(&self.preset).to_c(retained))
                }
                kAudioUnitProperty_SampleRate => put(out, size, config().sample_rate),
                kAudioUnitProperty_StreamFormat => {
                    let channels = self.channels(scope, element).unwrap_or(0);
                    put(out, size, stream_format(config().sample_rate, channels))
                }
                kAudioUnitProperty_ParameterList => match scope {
                    kAudioUnitScope_Global => put_all(out, size, &self.ids),
                    _ => put_all::<u32>(out, size, &[]),
                },
                kAudioUnitProperty_ParameterInfo => match self.param(element) {
                    Some(param) => {
                        let info = self.param_info(param);
                        let status = put(out, size, info);
                        if status != noErr && !info.cfNameString.is_null() {
                            CFRelease(info.cfNameString);
                        }
                        status
                    }
                    None => kAudioUnitErr_InvalidParameter,
                },
                kAudioUnitProperty_ParameterValueStrings => {
                    let Some(param) = self.param(element) else {
                        return kAudioUnitErr_InvalidParameter;
                    };
                    let (min, max) = bounds(param);
                    let mut names = Vec::new();
                    let mut value = min;
                    while value <= max && names.len() < 1_025 {
                        match cf::string(&param.plain_to_text(value)) {
                            Some(name) => names.push(name),
                            None => return kAudioUnitErr_CannotDoInCurrentContext,
                        }
                        value += 1.0;
                    }
                    put_object(out, size, cf::array(&names))
                }
                kAudioUnitProperty_ParameterStringFromValue => {
                    let Some(mut query) = take::<AudioUnitParameterStringFromValue>(out, *size)
                    else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    let Some(param) = self.param(query.inParamID) else {
                        return kAudioUnitErr_InvalidParameter;
                    };
                    let value = if query.inValue.is_null() {
                        param.plain()
                    } else {
                        f64::from(query.inValue.read_unaligned())
                    };
                    let Some(text) = cf::string(&param.plain_to_text(value)) else {
                        return kAudioUnitErr_CannotDoInCurrentContext;
                    };
                    query.outString = text.into_raw();
                    put(out, size, query)
                }
                kAudioUnitProperty_ParameterValueFromString => {
                    let Some(mut query) = take::<AudioUnitParameterValueFromString>(out, *size)
                    else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    let Some(param) = self.param(query.inParamID) else {
                        return kAudioUnitErr_InvalidParameter;
                    };
                    let Some(text) = cf::read_string(query.inString) else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    let Some(value) = param.text_to_plain(&text) else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    query.outValue = value as f32;
                    put(out, size, query)
                }
                kAudioUnitProperty_ElementCount => {
                    let count = match scope {
                        kAudioUnitScope_Input => self.inputs.len() as u32,
                        kAudioUnitScope_Output => 1 + P::AUX_OUTPUTS.len() as u32,
                        _ => 1,
                    };
                    put(out, size, count)
                }
                kAudioUnitProperty_Latency => {
                    let frames = f64::from(self.latency.load(Ordering::Relaxed));
                    put(out, size, frames / config().sample_rate)
                }
                kAudioUnitProperty_TailTime => put(out, size, 0.0f64),
                kAudioUnitProperty_SupportedNumChannels => {
                    put_all(out, size, &self.channel_infos())
                }
                kAudioUnitProperty_MIDIOutputCallbackInfo => {
                    // One MIDI output, named; the caller releases the list.
                    let Some(name) = cf::string("MIDI Out") else {
                        return kAudioUnitErr_CannotDoInCurrentContext;
                    };
                    put_object(out, size, cf::array(&[name]))
                }
                kAudioUnitProperty_MIDIOutputCallback => {
                    let callback = config().midi_out.unwrap_or(AUMIDIOutputCallbackStruct {
                        midiOutputCallback: None,
                        userData: ptr::null_mut(),
                    });
                    put(out, size, callback)
                }
                kAudioUnitProperty_MaximumFramesPerSlice => put(out, size, config().max_frames),
                kAudioUnitProperty_LastRenderError => {
                    put(out, size, self.last_error.swap(noErr, Ordering::Relaxed))
                }
                kAudioUnitProperty_HostCallbacks => match config().host {
                    Some(host) => put(out, size, host),
                    None => put(
                        out,
                        size,
                        HostCallbackInfo {
                            hostUserData: ptr::null_mut(),
                            beatAndTempoProc: None,
                            musicalTimeLocationProc: None,
                            transportStateProc: None,
                            transportStateProc2: None,
                        },
                    ),
                },
                kAudioUnitProperty_InPlaceProcessing => put(out, size, config().in_place),
                kAudioUnitProperty_BypassEffect => put(out, size, u32::from(config().bypass)),
                kAudioUnitProperty_ElementName => {
                    let name = match (scope, element) {
                        (kAudioUnitScope_Output, 0) => "Output",
                        (kAudioUnitScope_Output, aux) => P::AUX_OUTPUTS
                            .get(aux as usize - 1)
                            .map_or("Output", |aux| aux.name),
                        (_, 0) if P::LAYOUT.inputs > 0 => "Input",
                        _ => "Sidechain",
                    };
                    put_object(out, size, cf::string(name))
                }
                kAudioUnitProperty_SupportedChannelLayoutTags => {
                    let tags: Vec<AudioChannelLayoutTag> = self
                        .widths(scope, element)
                        .into_iter()
                        .map(layout_tag)
                        .collect();
                    put_all(out, size, &tags)
                }
                kAudioUnitProperty_AudioChannelLayout => {
                    let channels = self.channels(scope, element).unwrap_or(0);
                    put(
                        out,
                        size,
                        AudioChannelLayout {
                            mChannelLayoutTag: layout_tag(channels),
                            mChannelBitmap: 0,
                            mNumberChannelDescriptions: 0,
                        },
                    )
                }
                kAudioUnitProperty_ShouldAllocateBuffer => put(out, size, 1u32),
                // Write-only: the host sets them and reads nothing back.
                kAudioUnitProperty_SetRenderCallback | kAudioUnitProperty_MakeConnection => {
                    kAudioUnitErr_InvalidProperty
                }
                _ => kAudioUnitErr_InvalidProperty,
            }
        }
    }

    /// Answers `SetProperty`, and calls the listeners of what changed.
    ///
    /// # Safety
    ///
    /// `data` must be null or hold `size` readable bytes.
    unsafe fn set(
        &self,
        property: AudioUnitPropertyID,
        scope: AudioUnitScope,
        element: AudioUnitElement,
        data: *const c_void,
        size: u32,
    ) -> OSStatus {
        let writable = match self.describe(property, scope, element) {
            Ok((_, writable)) => writable,
            Err(status) => return status,
        };
        let initialized = self.initialized.load(Ordering::Acquire);
        // SAFETY: per this function's contract, for each `take`.
        let status = unsafe {
            match property {
                kAudioUnitProperty_StreamFormat => {
                    let Some(format) = take::<AudioStreamBasicDescription>(data, size) else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    // Any width the element has in a layout the plugin
                    // offers; whether the elements' widths add up to one is
                    // checked when the unit initialises.
                    let Some(channels) = self
                        .widths(scope, element)
                        .into_iter()
                        .find(|&channels| acceptable(&format, channels))
                    else {
                        return kAudioUnitErr_FormatNotSupported;
                    };
                    {
                        let mut config = self.config.take();
                        if width::<P>(config.layout, scope, element) != Ok(channels) {
                            if initialized {
                                return kAudioUnitErr_Initialized;
                            }
                            set_width::<P>(&mut config.layout, scope, element, channels);
                        }
                    }
                    self.set_sample_rate(format.mSampleRate, initialized)
                }
                kAudioUnitProperty_MIDIOutputCallback => {
                    let callback = take::<AUMIDIOutputCallbackStruct>(data, size);
                    if callback.is_none() && !data.is_null() {
                        return kAudioUnitErr_InvalidPropertyValue;
                    }
                    self.config.take().midi_out =
                        callback.filter(|callback| callback.midiOutputCallback.is_some());
                    noErr
                }
                kAudioUnitProperty_SampleRate => {
                    let Some(rate) = take::<f64>(data, size) else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    if !(rate.is_finite() && rate > 0.0) {
                        return kAudioUnitErr_InvalidPropertyValue;
                    }
                    self.set_sample_rate(rate, initialized)
                }
                kAudioUnitProperty_MaximumFramesPerSlice => {
                    let Some(frames) = take::<u32>(data, size) else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    if frames == 0 || frames > LARGEST_MAX_FRAMES {
                        return kAudioUnitErr_InvalidPropertyValue;
                    }
                    {
                        let mut config = self.config.take();
                        if config.max_frames == frames {
                            return noErr;
                        }
                        config.max_frames = frames;
                    }
                    if initialized {
                        self.prepare();
                    }
                    noErr
                }
                kAudioUnitProperty_ClassInfo => {
                    let Some(list) = take::<CFPropertyListRef>(data, size) else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    let status = self.restore(list);
                    if status == noErr {
                        self.changed(kAudioUnitProperty_PresentPreset, kAudioUnitScope_Global, 0);
                    }
                    status
                }
                kAudioUnitProperty_PresentPreset | kAudioUnitProperty_CurrentPreset => {
                    let Some(preset) = take::<AUPreset>(data, size) else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    // The name is copied into a string of the unit's own, so
                    // the caller keeps its reference and may change its
                    // string afterwards. A preset with no name is untitled.
                    let text = if preset.presetName.is_null() {
                        Some(UNTITLED.to_owned())
                    } else {
                        cf::read_string(preset.presetName)
                    };
                    let Some(text) = text else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    *lock(&self.preset) = Preset::named(preset.presetNumber, &text);
                    noErr
                }
                kAudioUnitProperty_SetRenderCallback => {
                    let source = match take::<AURenderCallbackStruct>(data, size) {
                        Some(callback) if callback.inputProc.is_some() => {
                            Source::Callback(callback)
                        }
                        _ if data.is_null()
                            || size as usize >= size_of::<AURenderCallbackStruct>() =>
                        {
                            Source::None
                        }
                        _ => return kAudioUnitErr_InvalidPropertyValue,
                    };
                    self.set_source(element, source)
                }
                kAudioUnitProperty_MakeConnection => {
                    let source = match take::<AudioUnitConnection>(data, size) {
                        Some(connection) if connection.destInputNumber != element => {
                            return kAudioUnitErr_InvalidPropertyValue;
                        }
                        Some(connection) if !connection.sourceAudioUnit.is_null() => {
                            Source::Connection(connection)
                        }
                        Some(_) => Source::None,
                        None if data.is_null() => Source::None,
                        None => return kAudioUnitErr_InvalidPropertyValue,
                    };
                    self.set_source(element, source)
                }
                kAudioUnitProperty_HostCallbacks => {
                    let mut host = HostCallbackInfo {
                        hostUserData: ptr::null_mut(),
                        beatAndTempoProc: None,
                        musicalTimeLocationProc: None,
                        transportStateProc: None,
                        transportStateProc2: None,
                    };
                    if !data.is_null() {
                        // An older host gives fewer of the functions.
                        let given = (size as usize).min(size_of::<HostCallbackInfo>());
                        ptr::copy_nonoverlapping(
                            data.cast::<u8>(),
                            (&raw mut host).cast::<u8>(),
                            given - given % size_of::<*const c_void>(),
                        );
                    }
                    self.config.take().host = (!data.is_null()).then_some(host);
                    noErr
                }
                kAudioUnitProperty_InPlaceProcessing => {
                    let Some(value) = take::<u32>(data, size) else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    // The plugin is always given its own memory; this only
                    // records what the host asked.
                    self.config.take().in_place = u32::from(value != 0);
                    noErr
                }
                kAudioUnitProperty_BypassEffect => {
                    let Some(value) = take::<u32>(data, size) else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    let bypass = value != 0;
                    let was = std::mem::replace(&mut self.config.take().bypass, bypass);
                    if was && !bypass {
                        // Coming back in, the plugin starts from silence.
                        self.audio.take().plugin.reset();
                    }
                    noErr
                }
                kAudioUnitProperty_AudioChannelLayout => {
                    let Some(layout) = take::<AudioChannelLayout>(data, size) else {
                        return kAudioUnitErr_InvalidPropertyValue;
                    };
                    let channels = self.channels(scope, element).unwrap_or(0);
                    if layout.mChannelLayoutTag == layout_tag(channels) {
                        noErr
                    } else {
                        kAudioUnitErr_FormatNotSupported
                    }
                }
                kAudioUnitProperty_ShouldAllocateBuffer => {
                    if take::<u32>(data, size).is_none() {
                        return kAudioUnitErr_InvalidPropertyValue;
                    }
                    noErr
                }
                _ if !writable => kAudioUnitErr_PropertyNotWritable,
                _ => kAudioUnitErr_InvalidProperty,
            }
        };
        if status == noErr {
            self.changed(property, scope, element);
            if property == kAudioUnitProperty_StreamFormat {
                self.changed(kAudioUnitProperty_SampleRate, scope, element);
            } else if property == kAudioUnitProperty_SampleRate {
                self.changed(kAudioUnitProperty_StreamFormat, scope, element);
            }
        }
        status
    }

    /// Sets the one sample rate every element runs at.
    fn set_sample_rate(&self, rate: f64, initialized: bool) -> OSStatus {
        let mut config = self.config.take();
        if config.sample_rate == rate {
            return noErr;
        }
        if initialized {
            return kAudioUnitErr_Initialized;
        }
        config.sample_rate = rate;
        noErr
    }

    fn set_source(&self, element: AudioUnitElement, source: Source) -> OSStatus {
        match self.config.take().sources.get_mut(element as usize) {
            Some(slot) => {
                *slot = source;
                noErr
            }
            None => kAudioUnitErr_InvalidElement,
        }
    }
}

unsafe extern "C" fn get_property_info<P: Plugin>(
    this: *mut c_void,
    property: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    size: *mut u32,
    writable: *mut Boolean,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: the host passes the pointer the factory returned.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        match this.describe(property, scope, element) {
            Ok((bytes, can_set)) => {
                // SAFETY: the host passes places for the answers, or null.
                unsafe {
                    if !size.is_null() {
                        *size = bytes as u32;
                    }
                    if !writable.is_null() {
                        *writable = Boolean::from(can_set);
                    }
                }
                noErr
            }
            Err(status) => status,
        }
    })
}

unsafe extern "C" fn get_property<P: Plugin>(
    this: *mut c_void,
    property: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    out: *mut c_void,
    size: *mut u32,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        if size.is_null() {
            return paramErr;
        }
        // SAFETY: the host passes `*size` writable bytes at `out`.
        unsafe { this.get(property, scope, element, out, size) }
    })
}

unsafe extern "C" fn set_property<P: Plugin>(
    this: *mut c_void,
    property: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    data: *const c_void,
    size: u32,
) -> OSStatus {
    guard(paramErr, || {
        // SAFETY: as above.
        let Some(this) = (unsafe { instance::<P>(this) }) else {
            return paramErr;
        };
        // SAFETY: the host passes `size` readable bytes at `data`, or null.
        unsafe { this.set(property, scope, element, data, size) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_format_is_the_canonical_one_and_others_are_refused() {
        let format = stream_format(48_000.0, 2);
        assert!(acceptable(&format, 2));
        assert!(!acceptable(&format, 1));
        let interleaved = AudioStreamBasicDescription {
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            ..format
        };
        assert!(!acceptable(&interleaved, 2));
        let integers = AudioStreamBasicDescription {
            mFormatFlags: kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonInterleaved,
            mBitsPerChannel: 16,
            ..format
        };
        assert!(!acceptable(&integers, 2));
        for rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(!acceptable(
                &AudioStreamBasicDescription {
                    mSampleRate: rate,
                    ..format
                },
                2
            ));
        }
        assert_eq!(layout_tag(1), kAudioChannelLayoutTag_Mono);
        assert_eq!(layout_tag(2), kAudioChannelLayoutTag_Stereo);
        assert_eq!(layout_tag(6), (147 << 16) | 6);
    }

    #[test]
    fn a_packet_list_is_laid_out_as_coremidi_reads_it_and_refuses_what_does_not_fit() {
        let mut words = [0u32; 6];
        let mut list = PacketList::new(&mut words);
        assert!(list.push(5, &[0x90, 60, 100]));
        let second =
            (MIDI_PACKET_LIST_PACKETS + MIDI_PACKET_DATA + 3).next_multiple_of(MIDI_PACKET_ALIGN);
        assert_eq!(list.end, second);
        // 24 bytes hold the count and one packet of three, not two.
        assert!(!list.push(6, &[0x80, 60, 0]));
        let bytes = list.finish().cast::<u8>();
        // SAFETY: the list's own 24 bytes.
        let bytes = unsafe { std::slice::from_raw_parts(bytes, 24) };
        assert_eq!(bytes[..4], 1u32.to_ne_bytes());
        assert_eq!(bytes[4..12], 5u64.to_ne_bytes());
        assert_eq!(bytes[12..14], 3u16.to_ne_bytes());
        assert_eq!(bytes[14..17], [0x90, 60, 100]);
        list.clear();
        assert!(list.push(6, &[0x80, 60, 0]));
        assert_eq!(list.count, 1);
    }

    #[test]
    fn names_are_cut_at_a_character_and_end() {
        let mut field = [1 as c_char; 6];
        copy_name("añadir", &mut field);
        // "aña" takes four bytes; "añad" would take five and the NUL a sixth.
        assert_eq!(field.map(|c| c as u8), [b'a', 0xC3, 0xB1, b'a', b'd', 0]);
        let mut short = [1 as c_char; 3];
        copy_name("éé", &mut short);
        assert_eq!(short.map(|c| c as u8), [0xC3, 0xA9, 0]);
    }
}
