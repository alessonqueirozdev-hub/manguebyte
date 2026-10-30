//! Acts as an Audio Unit host: makes the unit through its factory, looks
//! its methods up by selector, and drives it through them as AudioToolbox
//! would, with a counting allocator around every render.
//!
//! The plugins are written here, and also driven with no host at all, by
//! `manguebyte_plugin::Harness`; the unit has to sound the same, to the
//! bit. On macOS the CoreFoundation objects are CoreFoundation's; on other
//! systems they are the wrapper's stand-in, whose count of live objects
//! shows that nothing leaks.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ffi::{CStr, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use manguebyte_au::sys::AudioBuffer as AuBuffer;
use manguebyte_au::sys::*;
use manguebyte_au::{__private::factory, as_os_type, manufacturer_code, subtype_code};
use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::{Event, EventKind, Expression, NoteRef};
use manguebyte_dsp::note_input::{Mpe, NoteInput};
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_midi::MidiMessage;
use manguebyte_params::{BoolParam, ChoiceParam, FloatParam, FloatRange, Param, param_set};
use manguebyte_plugin::output::OutputBuffer;
use manguebyte_plugin::{AudioLayout, Harness, Plugin, PluginInfo, PluginKind};

// --- Watching the heap ---------------------------------------------------------

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
        let _ = WATCHING.try_with(|watching| {
            if watching.get() {
                let _ = HEAP_CALLS.try_with(|calls| calls.set(calls.get() + 1));
            }
        });
    }
}

// SAFETY: every method forwards to the system allocator unchanged; the only
// addition is bumping a thread-local counter, which does not allocate.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn heap_calls_during(work: impl FnOnce()) -> usize {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    HEAP_CALLS.with(Cell::get)
}

/// The tests run one at a time, so that the count of live objects of the
/// CoreFoundation stand-in belongs to one test.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

struct Serial {
    _held: MutexGuard<'static, ()>,
    #[cfg(not(target_os = "macos"))]
    live: usize,
}

fn serial() -> Serial {
    let held = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
    Serial {
        _held: held,
        #[cfg(not(target_os = "macos"))]
        live: manguebyte_au::fake_cf::LIVE.load(Ordering::Relaxed),
    }
}

impl Drop for Serial {
    fn drop(&mut self) {
        #[cfg(not(target_os = "macos"))]
        if !std::thread::panicking() {
            assert_eq!(
                manguebyte_au::fake_cf::LIVE.load(Ordering::Relaxed),
                self.live,
                "a CoreFoundation object leaked"
            );
        }
    }
}

// --- The plugins ----------------------------------------------------------------

struct FxParams {
    gain: FloatParam,
    mode: ChoiceParam,
    mute: BoolParam,
}
param_set!(FxParams { gain, mode, mute });

/// Multiplies its input by its gain, and in the mode "Key" adds half of
/// its sidechain. It follows the events, so every change lands on its
/// frame.
struct Fx {
    params: Arc<FxParams>,
    gain: f32,
    mode: usize,
    mute: bool,
}

impl Plugin for Fx {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.fx",
        name: "Fx",
        vendor: "Manguebyte",
        version: "1.2.3",
        description: "Turns things up",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 2,
        outputs: 2,
        sidechain: 1,
    };
    type Params = FxParams;

    fn new() -> Self {
        Self {
            params: Arc::new(FxParams {
                gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 4.0), 1.0)
                    .with_unit("dB"),
                mode: ChoiceParam::new("mode", "Mode", &["Plain", "Key"], 0),
                mute: BoolParam::new("mute", "Mute", false),
            }),
            gain: 1.0,
            mode: 0,
            mute: false,
        }
    }
    fn params(&self) -> Arc<FxParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {
        self.gain = self.params.gain.value();
        self.mode = self.params.mode.index();
        self.mute = self.params.mute.value();
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        let mut events = context.events.iter().peekable();
        for frame in 0..buffer.num_frames() {
            while let Some(event) = events.peek().filter(|event| event.offset as usize <= frame) {
                if let EventKind::ParamValue { param, value } = event.kind {
                    if param == self.params.gain.param_id() {
                        self.gain = value as f32;
                    } else if param == self.params.mode.param_id() {
                        self.mode = value as usize;
                    } else if param == self.params.mute.param_id() {
                        self.mute = value != 0.0;
                    }
                }
                events.next();
            }
            let key = context
                .sidechain
                .map_or(0.0, |sidechain| sidechain.channel(0)[frame]);
            for channel in 0..buffer.num_channels() {
                let sample = &mut buffer.channel_mut(channel)[frame];
                *sample = if self.mute { 0.0 } else { *sample * self.gain };
                if self.mode == 1 {
                    *sample += 0.5 * key;
                }
            }
        }
    }
}

struct KeysParams {
    level: FloatParam,
}
param_set!(KeysParams { level });

/// What the instrument last saw of the transport, for the tests.
static SEEN_TEMPO: AtomicU64 = AtomicU64::new(0);
static SEEN_POSITION: AtomicU64 = AtomicU64::new(0);

/// Plays a constant for each held key: the key's number over 128, times
/// its velocity and the level, from the frame its note starts to the
/// frame it ends, and a pitch bend adds its value over 8,192.
struct Keys {
    params: Arc<KeysParams>,
    held: [f32; 128],
    bend: f32,
}

impl Plugin for Keys {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.keys",
        name: "Keys",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays key numbers",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    type Params = KeysParams;

    fn new() -> Self {
        Self {
            params: Arc::new(KeysParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 1.0), 0.5),
            }),
            held: [0.0; 128],
            bend: 0.0,
        }
    }
    fn params(&self) -> Arc<KeysParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn reset(&mut self) {
        self.held = [0.0; 128];
        self.bend = 0.0;
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        SEEN_TEMPO.store(
            context.transport.tempo_bpm.unwrap_or(0.0).to_bits(),
            Ordering::Relaxed,
        );
        SEEN_POSITION.store(
            context.transport.position_beats.unwrap_or(-1.0).to_bits(),
            Ordering::Relaxed,
        );
        let level = self.params.level.value();
        let mut events = context.events.iter().peekable();
        for frame in 0..buffer.num_frames() {
            while let Some(event) = events.peek().filter(|event| event.offset as usize <= frame) {
                match event.kind {
                    EventKind::NoteOn { note, velocity } => {
                        self.held[usize::from(note.key)] = velocity;
                    }
                    EventKind::NoteOff { note, .. } => self.held[usize::from(note.key)] = 0.0,
                    EventKind::Midi(MidiMessage::PitchBend { value, .. }) => {
                        self.bend = f32::from(value) / 8_192.0;
                    }
                    _ => {}
                }
                events.next();
            }
            let mut sample = self.bend;
            for (key, velocity) in self.held.iter().enumerate() {
                sample += key as f32 / 128.0 * velocity * level;
            }
            for channel in 0..buffer.num_channels() {
                buffer.channel_mut(channel)[frame] = sample;
            }
        }
    }
}

// --- A host -----------------------------------------------------------------------

/// What the host told the unit it is, in `Open`.
static COMPONENT: u8 = 0;

/// What property listeners heard.
static HEARD: Mutex<Vec<(u32, u32, u32, usize)>> = Mutex::new(Vec::new());

unsafe extern "C" fn listener(
    user: *mut c_void,
    unit: AudioUnit,
    property: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
) {
    assert!(ptr::eq(unit.cast_const().cast::<u8>(), &COMPONENT));
    HEARD
        .lock()
        .unwrap()
        .push((property, scope, element, user as usize));
}

/// Audio a render callback hands the unit: a buffer, read from where the
/// last call left off.
struct Feed {
    audio: AudioBuffer<f32>,
    position: usize,
}

unsafe extern "C" fn feed(
    refcon: *mut c_void,
    _flags: *mut AudioUnitRenderActionFlags,
    _timestamp: *const AudioTimeStamp,
    _bus: u32,
    frames: u32,
    list: *mut AudioBufferList,
) -> OSStatus {
    // SAFETY: the refcon is the `Feed` the test set up, and the list the
    // unit's own, with as many buffers as its count says.
    unsafe {
        let feed = &mut *refcon.cast::<Feed>();
        let frames = frames as usize;
        let buffers = std::slice::from_raw_parts_mut(
            list.cast::<u8>()
                .add(AUDIO_BUFFER_LIST_BUFFERS)
                .cast::<AuBuffer>(),
            (*list).mNumberBuffers as usize,
        );
        for (channel, buffer) in buffers.iter_mut().enumerate() {
            let target = std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), frames);
            let source = &feed.audio.channel(channel)[feed.position..feed.position + frames];
            target.copy_from_slice(source);
        }
        feed.position += frames;
    }
    noErr
}

/// What render notifications saw: the flags of each call.
static NOTIFIED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

unsafe extern "C" fn notification(
    _refcon: *mut c_void,
    flags: *mut AudioUnitRenderActionFlags,
    _timestamp: *const AudioTimeStamp,
    _bus: u32,
    _frames: u32,
    _list: *mut AudioBufferList,
) -> OSStatus {
    // SAFETY: the unit passes its flags.
    NOTIFIED.lock().unwrap().push(unsafe { *flags });
    noErr
}

unsafe extern "C" fn beat_and_tempo(
    _user: *mut c_void,
    beat: *mut f64,
    tempo: *mut f64,
) -> OSStatus {
    // SAFETY: the unit passes places for the answers.
    unsafe {
        *beat = 7.5;
        *tempo = 96.0;
    }
    noErr
}

type GetPropertyInfo =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut u32, *mut Boolean) -> OSStatus;
type GetProperty =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut c_void, *mut u32) -> OSStatus;
type SetProperty = unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const c_void, u32) -> OSStatus;
type Simple = unsafe extern "C" fn(*mut c_void) -> OSStatus;
type Render = unsafe extern "C" fn(
    *mut c_void,
    *mut AudioUnitRenderActionFlags,
    *const AudioTimeStamp,
    u32,
    u32,
    *mut AudioBufferList,
) -> OSStatus;
type GetParameter = unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut f32) -> OSStatus;
type SetParameter = unsafe extern "C" fn(*mut c_void, u32, u32, u32, f32, u32) -> OSStatus;
type ScheduleParameters =
    unsafe extern "C" fn(*mut c_void, *const AudioUnitParameterEvent, u32) -> OSStatus;
type MidiEvent = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32) -> OSStatus;
type AddListener =
    unsafe extern "C" fn(*mut c_void, u32, AudioUnitPropertyListenerProc, *mut c_void) -> OSStatus;
type RemoveListener =
    unsafe extern "C" fn(*mut c_void, u32, AudioUnitPropertyListenerProc) -> OSStatus;
type RenderNotify = unsafe extern "C" fn(*mut c_void, AURenderCallback, *mut c_void) -> OSStatus;
type Reset = unsafe extern "C" fn(*mut c_void, u32, u32) -> OSStatus;

/// One unit, as a host holds it.
struct Host {
    this: *mut c_void,
}

impl Host {
    fn new<P: Plugin>() -> Self {
        let this = factory::<P>();
        assert!(!this.is_null(), "the factory made nothing");
        let host = Self { this };
        // SAFETY: the interface the factory returned, opened as AudioToolbox
        // opens it.
        let status =
            unsafe { (host.interface().Open)(this, (&raw const COMPONENT).cast_mut().cast()) };
        assert_eq!(status, noErr);
        host
    }

    fn interface(&self) -> &AudioComponentPlugInInterface {
        // SAFETY: the factory returned a pointer to the interface.
        unsafe { &*self.this.cast::<AudioComponentPlugInInterface>() }
    }

    fn lookup(&self, selector: i16) -> *const c_void {
        // SAFETY: the unit's own `Lookup`.
        unsafe { (self.interface().Lookup)(selector) }
    }

    /// The method for a selector, as the type the host calls it by.
    fn method<T: Copy>(&self, selector: i16) -> T {
        let found = self.lookup(selector);
        assert!(!found.is_null(), "no method for selector {selector:#x}");
        assert_eq!(size_of::<T>(), size_of::<*const c_void>());
        // SAFETY: a function pointer, as the host casts it to the method's
        // own type before it calls it.
        unsafe { std::mem::transmute_copy(&found) }
    }

    fn info(&self, property: u32, scope: u32, element: u32) -> Result<(u32, bool), OSStatus> {
        let call: GetPropertyInfo = self.method(kAudioUnitGetPropertyInfoSelect);
        let (mut size, mut writable) = (0u32, 0u8);
        // SAFETY: the method, with places for its answers.
        let status = unsafe {
            call(
                self.this,
                property,
                scope,
                element,
                &mut size,
                &mut writable,
            )
        };
        (status == noErr)
            .then_some((size, writable != 0))
            .ok_or(status)
    }

    fn get<T: Copy>(&self, property: u32, scope: u32, element: u32) -> Result<T, OSStatus> {
        let mut value = std::mem::MaybeUninit::<T>::zeroed();
        let mut size = size_of::<T>() as u32;
        let status = self.get_raw(
            property,
            scope,
            element,
            value.as_mut_ptr().cast(),
            &mut size,
        );
        if status != noErr {
            return Err(status);
        }
        assert_eq!(size as usize, size_of::<T>(), "property {property}");
        // SAFETY: the unit wrote a whole `T`.
        Ok(unsafe { value.assume_init() })
    }

    fn get_raw(
        &self,
        property: u32,
        scope: u32,
        element: u32,
        out: *mut c_void,
        size: &mut u32,
    ) -> OSStatus {
        let call: GetProperty = self.method(kAudioUnitGetPropertySelect);
        // SAFETY: the method, with room for `size` bytes.
        unsafe { call(self.this, property, scope, element, out, size) }
    }

    fn set<T>(&self, property: u32, scope: u32, element: u32, value: &T) -> OSStatus {
        let call: SetProperty = self.method(kAudioUnitSetPropertySelect);
        // SAFETY: the method, with a value of the size given.
        unsafe {
            call(
                self.this,
                property,
                scope,
                element,
                ptr::from_ref(value).cast(),
                size_of::<T>() as u32,
            )
        }
    }

    fn simple(&self, selector: i16) -> OSStatus {
        let call: Simple = self.method(selector);
        // SAFETY: the method.
        unsafe { call(self.this) }
    }

    fn parameter(&self, id: ParamId) -> Result<f32, OSStatus> {
        let call: GetParameter = self.method(kAudioUnitGetParameterSelect);
        let mut value = f32::NAN;
        // SAFETY: the method, with a place for the value.
        let status = unsafe { call(self.this, id.0, kAudioUnitScope_Global, 0, &mut value) };
        (status == noErr).then_some(value).ok_or(status)
    }

    fn set_parameter(&self, id: ParamId, value: f32) -> OSStatus {
        let call: SetParameter = self.method(kAudioUnitSetParameterSelect);
        // SAFETY: the method.
        unsafe { call(self.this, id.0, kAudioUnitScope_Global, 0, value, 0) }
    }

    fn schedule(&self, events: &[AudioUnitParameterEvent]) -> OSStatus {
        let call: ScheduleParameters = self.method(kAudioUnitScheduleParametersSelect);
        // SAFETY: the method, with that many events.
        unsafe { call(self.this, events.as_ptr(), events.len() as u32) }
    }

    fn midi(&self, bytes: [u8; 3], offset: u32) -> OSStatus {
        let call: MidiEvent = self.method(kMusicDeviceMIDIEventSelect);
        // SAFETY: the method.
        unsafe {
            call(
                self.this,
                u32::from(bytes[0]),
                u32::from(bytes[1]),
                u32::from(bytes[2]),
                offset,
            )
        }
    }

    fn feed(&self, element: u32, feed: &mut Feed) -> OSStatus {
        let callback = AURenderCallbackStruct {
            inputProc: Some(self::feed),
            inputProcRefCon: ptr::from_mut(feed).cast(),
        };
        self.set(
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Input,
            element,
            &callback,
        )
    }

    /// Renders `frames` frames into `output` from frame `at`, in the host's
    /// memory or, with `own_memory`, in the unit's.
    fn render(
        &self,
        frames: u32,
        output: &mut AudioBuffer<f32>,
        at: usize,
        own_memory: bool,
    ) -> OSStatus {
        let call: Render = self.method(kAudioUnitRenderSelect);
        let channels = output.num_channels();
        let mut words = vec![0u64; 1 + 2 * channels];
        let list = words.as_mut_ptr().cast::<AudioBufferList>();
        // SAFETY: the words hold the header and `channels` buffers.
        let buffers = unsafe {
            (*list).mNumberBuffers = channels as u32;
            std::slice::from_raw_parts_mut(
                list.cast::<u8>()
                    .add(AUDIO_BUFFER_LIST_BUFFERS)
                    .cast::<AuBuffer>(),
                channels,
            )
        };
        for (buffer, samples) in buffers.iter_mut().zip(output.iter_channels_mut()) {
            *buffer = AuBuffer {
                mNumberChannels: 1,
                mDataByteSize: frames * 4,
                mData: if own_memory {
                    ptr::null_mut()
                } else {
                    samples[at..].as_mut_ptr().cast()
                },
            };
        }
        let mut flags: AudioUnitRenderActionFlags = 0;
        let stamp = AudioTimeStamp::default();
        let mut status = noErr;
        let heap = heap_calls_during(|| {
            // SAFETY: the method, with a list of `channels` buffers of
            // `frames` frames or none.
            status = unsafe { call(self.this, &mut flags, &stamp, 0, frames, list) };
        });
        assert_eq!(heap, 0, "the unit used the heap while rendering ({status})");
        if status == noErr && own_memory {
            for (buffer, samples) in buffers.iter().zip(output.iter_channels_mut()) {
                assert!(!buffer.mData.is_null(), "the unit gave no memory");
                assert_eq!(buffer.mDataByteSize, frames * 4);
                // SAFETY: the unit's memory, valid until the next render.
                let given = unsafe {
                    std::slice::from_raw_parts(buffer.mData.cast::<f32>(), frames as usize)
                };
                samples[at..at + frames as usize].copy_from_slice(given);
            }
        }
        status
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // SAFETY: the interface, closed once, as AudioToolbox closes it.
        let status = unsafe { (self.interface().Close)(self.this) };
        assert_eq!(status, noErr);
    }
}

fn immediate(param: ParamId, offset: u32, value: f32) -> AudioUnitParameterEvent {
    AudioUnitParameterEvent {
        scope: kAudioUnitScope_Global,
        element: 0,
        parameter: param.0,
        eventType: kParameterEvent_Immediate,
        eventValues: [offset, value.to_bits(), 0, 0],
    }
}

fn signal(channels: usize, frames: usize, speed: f32) -> AudioBuffer<f32> {
    let mut buffer = AudioBuffer::new(channels, frames);
    for (index, channel) in buffer.iter_channels_mut().enumerate() {
        for (frame, sample) in channel.iter_mut().enumerate() {
            *sample = 0.5 * (frame as f32 * (speed + 0.01 * index as f32)).sin();
        }
    }
    buffer
}

fn assert_same_bits(actual: &AudioBuffer<f32>, expected: &AudioBuffer<f32>, what: &str) {
    for channel in 0..expected.num_channels() {
        let (ours, theirs) = (actual.channel(channel), expected.channel(channel));
        if let Some(frame) = (0..ours.len()).find(|&f| ours[f].to_bits() != theirs[f].to_bits()) {
            panic!(
                "{what}: channel {channel} differs from frame {frame}: {} against {}",
                ours[frame], theirs[frame]
            );
        }
    }
}

const GAIN: ParamId = ParamId::from_name("gain");
const MODE: ParamId = ParamId::from_name("mode");
const MUTE: ParamId = ParamId::from_name("mute");

// --- The tests ----------------------------------------------------------------------

#[test]
fn the_unit_has_the_methods_of_its_kind() {
    let _serial = serial();
    let fx = Host::new::<Fx>();
    for selector in [
        kAudioUnitInitializeSelect,
        kAudioUnitUninitializeSelect,
        kAudioUnitGetPropertyInfoSelect,
        kAudioUnitGetPropertySelect,
        kAudioUnitSetPropertySelect,
        kAudioUnitAddPropertyListenerSelect,
        kAudioUnitRemovePropertyListenerSelect,
        kAudioUnitRemovePropertyListenerWithUserDataSelect,
        kAudioUnitAddRenderNotifySelect,
        kAudioUnitRemoveRenderNotifySelect,
        kAudioUnitGetParameterSelect,
        kAudioUnitSetParameterSelect,
        kAudioUnitScheduleParametersSelect,
        kAudioUnitRenderSelect,
        kAudioUnitResetSelect,
    ] {
        assert!(!fx.lookup(selector).is_null(), "{selector:#x}");
    }
    // An effect takes no MIDI; an instrument does.
    assert!(fx.lookup(kMusicDeviceMIDIEventSelect).is_null());
    assert!(fx.lookup(0x7777).is_null());
    let keys = Host::new::<Keys>();
    assert!(!keys.lookup(kMusicDeviceMIDIEventSelect).is_null());
    assert!(!keys.lookup(kMusicDeviceSysExSelect).is_null());
}

#[test]
fn properties_describe_the_unit() {
    let _serial = serial();
    let fx = Host::new::<Fx>();
    use std::result::Result::{Err, Ok};
    let (input, output, global) = (
        kAudioUnitScope_Input,
        kAudioUnitScope_Output,
        kAudioUnitScope_Global,
    );

    // Elements: main input and sidechain, one output.
    assert_eq!(
        fx.get::<u32>(kAudioUnitProperty_ElementCount, input, 0),
        Ok(2)
    );
    assert_eq!(
        fx.get::<u32>(kAudioUnitProperty_ElementCount, output, 0),
        Ok(1)
    );
    let format = fx
        .get::<AudioStreamBasicDescription>(kAudioUnitProperty_StreamFormat, input, 1)
        .unwrap();
    assert_eq!(
        (format.mSampleRate, format.mChannelsPerFrame),
        (44_100.0, 1)
    );
    assert_eq!(format.mFormatID, kAudioFormatLinearPCM);
    assert_eq!(format.mBitsPerChannel, 32);
    assert_ne!(format.mFormatFlags & kAudioFormatFlagIsNonInterleaved, 0);
    assert_eq!(
        fx.info(kAudioUnitProperty_StreamFormat, input, 2),
        Err(kAudioUnitErr_InvalidElement)
    );
    assert_eq!(
        fx.info(kAudioUnitProperty_StreamFormat, global, 0),
        Err(kAudioUnitErr_InvalidScope)
    );
    assert_eq!(
        fx.get::<AUChannelInfo>(kAudioUnitProperty_SupportedNumChannels, global, 0),
        Ok(AUChannelInfo {
            inChannels: 2,
            outChannels: 2
        })
    );
    assert_eq!(
        fx.get::<u32>(kAudioUnitProperty_MaximumFramesPerSlice, global, 0),
        Ok(1_156)
    );
    assert_eq!(
        fx.get::<f64>(kAudioUnitProperty_Latency, global, 0),
        Ok(0.0)
    );
    assert_eq!(
        fx.get::<f64>(kAudioUnitProperty_TailTime, global, 0),
        Ok(0.0)
    );
    assert_eq!(
        fx.get::<AudioChannelLayoutTag>(kAudioUnitProperty_SupportedChannelLayoutTags, output, 0),
        Ok(kAudioChannelLayoutTag_Stereo)
    );
    assert_eq!(
        fx.get::<AudioChannelLayoutTag>(kAudioUnitProperty_SupportedChannelLayoutTags, input, 1),
        Ok(kAudioChannelLayoutTag_Mono)
    );
    assert_eq!(
        fx.info(0x7FFF, global, 0),
        Err(kAudioUnitErr_InvalidProperty)
    );

    // Parameters, in the plugin's order, each in its own unit and range.
    let (size, writable) = fx
        .info(kAudioUnitProperty_ParameterList, global, 0)
        .unwrap();
    assert_eq!((size, writable), (12, false));
    let ids = fx
        .get::<[u32; 3]>(kAudioUnitProperty_ParameterList, global, 0)
        .unwrap();
    assert_eq!(ids, [GAIN.0, MODE.0, MUTE.0]);
    let gain = fx
        .get::<AudioUnitParameterInfo>(kAudioUnitProperty_ParameterInfo, global, GAIN.0)
        .unwrap();
    // SAFETY: the name field ends with a NUL.
    assert_eq!(unsafe { CStr::from_ptr(gain.name.as_ptr()) }, c"Gain");
    assert_eq!(
        (gain.minValue, gain.maxValue, gain.defaultValue),
        (0.0, 4.0, 1.0)
    );
    assert_eq!(gain.unit, kAudioUnitParameterUnit_Decibels);
    let flags = kAudioUnitParameterFlag_IsReadable
        | kAudioUnitParameterFlag_IsWritable
        | kAudioUnitParameterFlag_HasCFNameString
        | kAudioUnitParameterFlag_CFNameRelease;
    assert_eq!(gain.flags & flags, flags);
    assert!(!gain.cfNameString.is_null());
    // SAFETY: the host releases the name, as the flags say.
    unsafe { CFRelease(gain.cfNameString) };
    let mode = fx
        .get::<AudioUnitParameterInfo>(kAudioUnitProperty_ParameterInfo, global, MODE.0)
        .unwrap();
    assert_eq!(mode.unit, kAudioUnitParameterUnit_Indexed);
    assert_eq!((mode.minValue, mode.maxValue), (0.0, 1.0));
    // SAFETY: as above.
    unsafe { CFRelease(mode.cfNameString) };
    let mute = fx
        .get::<AudioUnitParameterInfo>(kAudioUnitProperty_ParameterInfo, global, MUTE.0)
        .unwrap();
    assert_eq!(mute.unit, kAudioUnitParameterUnit_Boolean);
    // SAFETY: as above.
    unsafe { CFRelease(mute.cfNameString) };
    assert_eq!(
        fx.info(kAudioUnitProperty_ParameterInfo, global, 12_345),
        Err(kAudioUnitErr_InvalidParameter)
    );

    // The names of an indexed parameter's values.
    let names = fx
        .get::<CFArrayRef>(kAudioUnitProperty_ParameterValueStrings, global, MODE.0)
        .unwrap();
    assert!(!names.is_null());
    #[cfg(not(target_os = "macos"))]
    {
        // SAFETY: an array the unit made.
        let values = unsafe { manguebyte_au::fake_cf::array_values(names) };
        let read = |text: CFTypeRef| {
            let mut buffer = [0 as std::ffi::c_char; 32];
            // SAFETY: a string the unit made, and a buffer of that size.
            unsafe { CFStringGetCString(text, buffer.as_mut_ptr(), 32, kCFStringEncodingUTF8) };
            // SAFETY: the buffer ends with a NUL.
            unsafe { CStr::from_ptr(buffer.as_ptr()) }
                .to_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            values.iter().map(|&value| read(value)).collect::<Vec<_>>(),
            ["Plain", "Key"]
        );
    }
    // SAFETY: the host releases what it was given.
    unsafe { CFRelease(names) };
    assert_eq!(
        fx.info(kAudioUnitProperty_ParameterValueStrings, global, GAIN.0),
        Err(kAudioUnitErr_InvalidProperty)
    );

    // Values as text and back.
    let value = 2.5f32;
    let query = AudioUnitParameterStringFromValue {
        inParamID: GAIN.0,
        inValue: &value,
        outString: ptr::null(),
    };
    let mut answer = query;
    let mut size = size_of::<AudioUnitParameterStringFromValue>() as u32;
    let status = fx.get_raw(
        kAudioUnitProperty_ParameterStringFromValue,
        global,
        0,
        (&raw mut answer).cast(),
        &mut size,
    );
    assert_eq!(status, noErr);
    let text = answer.outString;
    assert!(!text.is_null());
    let mut back = AudioUnitParameterValueFromString {
        inParamID: GAIN.0,
        inString: text,
        outValue: f32::NAN,
    };
    let mut size = size_of::<AudioUnitParameterValueFromString>() as u32;
    let status = fx.get_raw(
        kAudioUnitProperty_ParameterValueFromString,
        global,
        0,
        (&raw mut back).cast(),
        &mut size,
    );
    assert_eq!(status, noErr);
    assert_eq!(back.outValue, 2.5);
    // SAFETY: the host releases the string it was given.
    unsafe { CFRelease(text) };

    // Parameters from any thread, held to their range.
    assert_eq!(fx.parameter(GAIN), Ok(1.0));
    assert_eq!(fx.set_parameter(GAIN, 9.0), noErr);
    assert_eq!(fx.parameter(GAIN), Ok(4.0));
    assert_eq!(
        fx.parameter(ParamId(7)),
        Err(kAudioUnitErr_InvalidParameter)
    );
    assert_eq!(
        fx.set_parameter(GAIN, f32::NAN),
        kAudioUnitErr_InvalidPropertyValue
    );
}

#[test]
fn formats_change_only_while_uninitialized_and_listeners_hear_it() {
    let _serial = serial();
    HEARD.lock().unwrap().clear();
    let fx = Host::new::<Fx>();
    let add: AddListener = fx.method(kAudioUnitAddPropertyListenerSelect);
    let user = 5 as *mut c_void;
    // SAFETY: the method, with a listener that outlives the unit.
    let status = unsafe {
        add(
            fx.this,
            kAudioUnitProperty_StreamFormat,
            Some(listener),
            user,
        )
    };
    assert_eq!(status, noErr);
    let output = kAudioUnitScope_Output;
    let mut format = fx
        .get::<AudioStreamBasicDescription>(kAudioUnitProperty_StreamFormat, output, 0)
        .unwrap();
    format.mSampleRate = 96_000.0;
    assert_eq!(
        fx.set(kAudioUnitProperty_StreamFormat, output, 0, &format),
        noErr
    );
    assert_eq!(
        fx.get::<f64>(kAudioUnitProperty_SampleRate, kAudioUnitScope_Input, 0),
        Ok(96_000.0)
    );
    assert_eq!(
        *HEARD.lock().unwrap(),
        [(kAudioUnitProperty_StreamFormat, output, 0, 5)]
    );
    // The channel counts are the plugin's.
    let mono = AudioStreamBasicDescription {
        mChannelsPerFrame: 1,
        ..format
    };
    assert_eq!(
        fx.set(kAudioUnitProperty_StreamFormat, output, 0, &mono),
        kAudioUnitErr_FormatNotSupported
    );
    assert_eq!(
        fx.set(
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
            &512u32
        ),
        noErr
    );
    assert_eq!(
        fx.set(
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
            &0u32
        ),
        kAudioUnitErr_InvalidPropertyValue
    );

    assert_eq!(fx.simple(kAudioUnitInitializeSelect), noErr);
    assert_eq!(
        fx.simple(kAudioUnitInitializeSelect),
        noErr,
        "a second time"
    );
    assert_eq!(
        fx.info(kAudioUnitProperty_StreamFormat, output, 0),
        Ok((40, false))
    );
    format.mSampleRate = 48_000.0;
    assert_eq!(
        fx.set(kAudioUnitProperty_StreamFormat, output, 0, &format),
        kAudioUnitErr_Initialized
    );
    format.mSampleRate = 96_000.0;
    assert_eq!(
        fx.set(kAudioUnitProperty_StreamFormat, output, 0, &format),
        noErr,
        "no change"
    );
    // The longest slice may change at any time, as Apple's base classes
    // allow: the unit is prepared again, and renders the new length.
    let mut main = Feed {
        audio: signal(2, 4_096, 0.1),
        position: 0,
    };
    assert_eq!(fx.feed(0, &mut main), noErr);
    let mut out = AudioBuffer::<f32>::new(2, 2_048);
    assert_eq!(
        fx.render(1_024, &mut out, 0, false),
        kAudioUnitErr_TooManyFramesToProcess
    );
    let max = kAudioUnitProperty_MaximumFramesPerSlice;
    assert_eq!(fx.info(max, kAudioUnitScope_Global, 0), Ok((4, true)));
    assert_eq!(fx.set(max, kAudioUnitScope_Global, 0, &1024u32), noErr);
    assert_eq!(fx.get::<u32>(max, kAudioUnitScope_Global, 0), Ok(1_024));
    assert_eq!(fx.render(1_024, &mut out, 0, false), noErr);
    assert_eq!(fx.simple(kAudioUnitUninitializeSelect), noErr);

    let remove: RemoveListener = fx.method(kAudioUnitRemovePropertyListenerSelect);
    // SAFETY: the method.
    let status = unsafe { remove(fx.this, kAudioUnitProperty_StreamFormat, Some(listener)) };
    assert_eq!(status, noErr);
    HEARD.lock().unwrap().clear();
    format.mSampleRate = 48_000.0;
    assert_eq!(
        fx.set(kAudioUnitProperty_StreamFormat, output, 0, &format),
        noErr
    );
    assert!(HEARD.lock().unwrap().is_empty());
}

#[test]
fn the_effect_sounds_as_it_does_with_no_host() {
    let _serial = serial();
    let frames = 4_096;
    let input = signal(2, frames, 0.05);
    let key = signal(1, frames, 0.003);
    for (slice, max) in [(512u32, 512u32), (64, 1_156), (1_000, 1_156)] {
        let what = format!("slices of {slice}");
        let fx = Host::new::<Fx>();
        assert_eq!(
            fx.set(
                kAudioUnitProperty_MaximumFramesPerSlice,
                kAudioUnitScope_Global,
                0,
                &max
            ),
            noErr
        );
        let mut main = Feed {
            audio: input.clone(),
            position: 0,
        };
        let mut side = Feed {
            audio: key.clone(),
            position: 0,
        };
        assert_eq!(fx.feed(0, &mut main), noErr);
        assert_eq!(fx.feed(1, &mut side), noErr);
        assert_eq!(fx.simple(kAudioUnitInitializeSelect), noErr);

        // The harness gets the same changes on the same frames.
        let mut harness =
            Harness::<Fx>::new(44_100.0, slice as usize).with_block_pattern(&[slice as usize]);
        let mut output = AudioBuffer::<f32>::new(2, frames);
        let mut done = 0usize;
        let mut slices = 0usize;
        while done < frames {
            let length = (slice as usize).min(frames - done);
            let at = done as u64;
            match slices {
                1 => {
                    // Sample-accurate changes in this slice.
                    assert_eq!(
                        fx.schedule(&[immediate(GAIN, 10, 0.5), immediate(MODE, 20, 1.0)]),
                        noErr
                    );
                    harness.schedule(
                        at + 10,
                        EventKind::ParamValue {
                            param: GAIN,
                            value: 0.5,
                        },
                    );
                    harness.schedule(
                        at + 20,
                        EventKind::ParamValue {
                            param: MODE,
                            value: 1.0,
                        },
                    );
                }
                3 => {
                    // A change from another thread: the next slice's first frame.
                    assert_eq!(fx.set_parameter(MUTE, 1.0), noErr);
                    harness.schedule(
                        at,
                        EventKind::ParamValue {
                            param: MUTE,
                            value: 1.0,
                        },
                    );
                }
                5 => {
                    assert_eq!(fx.set_parameter(MUTE, 0.0), noErr);
                    harness.schedule(
                        at,
                        EventKind::ParamValue {
                            param: MUTE,
                            value: 0.0,
                        },
                    );
                }
                _ => {}
            }
            let own_memory = slices % 2 == 1;
            assert_eq!(
                fx.render(length as u32, &mut output, done, own_memory),
                noErr,
                "{what}"
            );
            done += length;
            slices += 1;
        }
        let expected = harness.process_with_sidechain(&input, &key);
        assert_same_bits(&output, &expected, &what);
        assert_eq!(fx.parameter(MODE), Ok(1.0));
    }
}

/// An input callback that asks the unit for two properties, as a host may
/// while the unit pulls its input, before it hands the audio over.
struct Asking {
    feed: Feed,
    host: *const Host,
    answers: Vec<(Result<u32, OSStatus>, Result<u32, OSStatus>)>,
}

unsafe extern "C" fn asking(
    refcon: *mut c_void,
    flags: *mut AudioUnitRenderActionFlags,
    timestamp: *const AudioTimeStamp,
    bus: u32,
    frames: u32,
    list: *mut AudioBufferList,
) -> OSStatus {
    // SAFETY: the refcon is the `Asking` the test set up.
    let asking = unsafe { &mut *refcon.cast::<Asking>() };
    // SAFETY: the host the test made, which outlives the render.
    let host = unsafe { &*asking.host };
    asking.answers.push((
        host.get::<u32>(
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
        ),
        host.get::<u32>(kAudioUnitProperty_BypassEffect, kAudioUnitScope_Global, 0),
    ));
    // SAFETY: the unit's own arguments, passed on.
    unsafe {
        feed(
            (&raw mut asking.feed).cast(),
            flags,
            timestamp,
            bus,
            frames,
            list,
        )
    }
}

#[test]
fn an_input_callback_may_ask_the_unit_for_its_properties() {
    let _serial = serial();
    let (done, finished) = std::sync::mpsc::channel();
    // On a thread of its own: a unit that waits for itself never returns.
    std::thread::spawn(move || {
        let fx = Host::new::<Fx>();
        let mut asking = Asking {
            feed: Feed {
                audio: signal(2, 256, 0.05),
                position: 0,
            },
            host: &raw const fx,
            // Room made ahead: the render is watched for the heap.
            answers: Vec::with_capacity(4),
        };
        let callback = AURenderCallbackStruct {
            inputProc: Some(self::asking),
            inputProcRefCon: (&raw mut asking).cast(),
        };
        assert_eq!(
            fx.set(
                kAudioUnitProperty_SetRenderCallback,
                kAudioUnitScope_Input,
                0,
                &callback
            ),
            noErr
        );
        assert_eq!(fx.simple(kAudioUnitInitializeSelect), noErr);
        let mut output = AudioBuffer::<f32>::new(2, 256);
        let status = fx.render(256, &mut output, 0, false);
        let answers = std::mem::take(&mut asking.answers);
        // Closed before the answer goes back, so that the leak check the
        // test makes on its way out sees the unit gone.
        drop(fx);
        done.send((status, answers)).unwrap();
    });
    let (status, answers) = finished
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the render waited for itself");
    assert_eq!(status, noErr);
    assert_eq!(answers.len(), 1);
    assert!(answers[0].0.is_ok(), "{answers:?}");
    assert_eq!(answers[0].1, Ok(0), "{answers:?}");
}

#[test]
fn a_render_that_cannot_be_done_says_why() {
    let _serial = serial();
    let fx = Host::new::<Fx>();
    let mut output = AudioBuffer::<f32>::new(2, 2_048);
    assert_eq!(
        fx.render(64, &mut output, 0, false),
        kAudioUnitErr_Uninitialized
    );
    assert_eq!(fx.simple(kAudioUnitInitializeSelect), noErr);
    // An effect with nothing at its input.
    assert_eq!(
        fx.render(64, &mut output, 0, false),
        kAudioUnitErr_NoConnection
    );
    assert_eq!(
        fx.get::<OSStatus>(
            kAudioUnitProperty_LastRenderError,
            kAudioUnitScope_Global,
            0
        ),
        Ok(kAudioUnitErr_NoConnection)
    );
    let mut main = Feed {
        audio: signal(2, 4_096, 0.1),
        position: 0,
    };
    assert_eq!(fx.feed(0, &mut main), noErr);
    assert_eq!(
        fx.render(64, &mut output, 0, false),
        noErr,
        "the sidechain may stay unconnected"
    );
    assert_eq!(
        fx.render(2_000, &mut output, 0, false),
        kAudioUnitErr_TooManyFramesToProcess
    );
    let call: Render = fx.method(kAudioUnitRenderSelect);
    // SAFETY: the method, asked for a bus the unit does not have.
    let status = unsafe {
        call(
            fx.this,
            ptr::null_mut(),
            ptr::null(),
            1,
            64,
            ptr::null_mut(),
        )
    };
    assert_eq!(status, kAudioUnitErr_InvalidElement);

    // Render notifications come before and after.
    NOTIFIED.lock().unwrap().clear();
    // Room beforehand: the notification is the host's code, and runs inside
    // the render, where the heap is watched.
    NOTIFIED.lock().unwrap().reserve(16);
    let add: RenderNotify = fx.method(kAudioUnitAddRenderNotifySelect);
    // SAFETY: the method, with a notification that outlives the unit.
    let status = unsafe { add(fx.this, Some(notification), ptr::null_mut()) };
    assert_eq!(status, noErr);
    assert_eq!(fx.render(64, &mut output, 0, false), noErr);
    assert_eq!(
        *NOTIFIED.lock().unwrap(),
        [
            kAudioUnitRenderAction_PreRender,
            kAudioUnitRenderAction_PostRender
        ]
    );
    let remove: RenderNotify = fx.method(kAudioUnitRemoveRenderNotifySelect);
    // SAFETY: as above.
    let status = unsafe { remove(fx.this, Some(notification), ptr::null_mut()) };
    assert_eq!(status, noErr);
    assert_eq!(fx.render(64, &mut output, 0, false), noErr);
    assert_eq!(NOTIFIED.lock().unwrap().len(), 2);

    // Bypassed, the input comes through.
    assert_eq!(fx.set_parameter(GAIN, 0.0), noErr);
    assert_eq!(
        fx.set(
            kAudioUnitProperty_BypassEffect,
            kAudioUnitScope_Global,
            0,
            &1u32
        ),
        noErr
    );
    let before = main.position;
    assert_eq!(fx.render(64, &mut output, 0, false), noErr);
    assert_eq!(
        &output.channel(0)[..64],
        &main.audio.channel(0)[before..before + 64]
    );
}

#[test]
fn the_instrument_plays_midi_on_its_frames_and_reads_the_transport() {
    let _serial = serial();
    let keys = Host::new::<Keys>();
    assert_eq!(
        keys.get::<u32>(kAudioUnitProperty_ElementCount, kAudioUnitScope_Input, 0),
        Ok(0)
    );
    assert_eq!(
        keys.info(kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 0),
        Err(kAudioUnitErr_InvalidElement)
    );
    assert_eq!(
        keys.get::<AUChannelInfo>(
            kAudioUnitProperty_SupportedNumChannels,
            kAudioUnitScope_Global,
            0
        ),
        Ok(AUChannelInfo {
            inChannels: 0,
            outChannels: 2
        })
    );
    let host = HostCallbackInfo {
        hostUserData: ptr::null_mut(),
        beatAndTempoProc: Some(beat_and_tempo),
        musicalTimeLocationProc: None,
        transportStateProc: None,
        transportStateProc2: None,
    };
    assert_eq!(
        keys.set(
            kAudioUnitProperty_HostCallbacks,
            kAudioUnitScope_Global,
            0,
            &host
        ),
        noErr
    );
    assert_eq!(keys.simple(kAudioUnitInitializeSelect), noErr);

    let frames = 2_048;
    let mut output = AudioBuffer::<f32>::new(2, frames);
    let mut harness = Harness::<Keys>::new(44_100.0, 256);
    let notes: [([u8; 3], u64); 5] = [
        ([0x90, 60, 100], 10),
        ([0x90, 64, 127], 300),
        ([0xE0, 0, 80], 301),
        ([0x80, 60, 0], 700),
        ([0x90, 64, 0], 1_500),
    ];
    let mut done = 0usize;
    while done < frames {
        for &(bytes, frame) in &notes {
            if (done as u64..done as u64 + 256).contains(&frame) {
                assert_eq!(keys.midi(bytes, (frame - done as u64) as u32), noErr);
                let message = MidiMessage::from_bytes(&bytes).unwrap();
                harness.schedule(frame, Event::from_midi(0, message).kind);
            }
        }
        assert_eq!(keys.render(256, &mut output, done, false), noErr);
        done += 256;
    }
    // What the unit's plugin saw of the host's transport, before the plugin
    // with no host plays and sees its own.
    assert_eq!(f64::from_bits(SEEN_TEMPO.load(Ordering::Relaxed)), 96.0);
    assert_eq!(f64::from_bits(SEEN_POSITION.load(Ordering::Relaxed)), 7.5);
    let expected = harness.render(frames);
    assert_same_bits(&output, &expected, "the notes");
    assert!(output.channel(0).iter().any(|&sample| sample != 0.0));

    // A reset stops what was sounding.
    assert_eq!(keys.midi([0x90, 70, 127], 0), noErr);
    assert_eq!(keys.render(256, &mut output, 0, false), noErr);
    assert!(output.channel(0)[..256].iter().all(|&sample| sample != 0.0));
    let reset: Reset = keys.method(kAudioUnitResetSelect);
    // SAFETY: the method.
    let status = unsafe { reset(keys.this, kAudioUnitScope_Global, 0) };
    assert_eq!(status, noErr);
    assert_eq!(keys.render(256, &mut output, 0, false), noErr);
    assert!(output.channel(0)[..256].iter().all(|&sample| sample == 0.0));
}

#[test]
fn the_state_goes_out_and_comes_back_and_another_units_is_refused() {
    let _serial = serial();
    let fx = Host::new::<Fx>();
    let global = kAudioUnitScope_Global;
    assert_eq!(fx.set_parameter(GAIN, 3.0), noErr);
    assert_eq!(fx.set_parameter(MODE, 1.0), noErr);
    let saved = fx
        .get::<CFPropertyListRef>(kAudioUnitProperty_ClassInfo, global, 0)
        .unwrap();
    assert!(!saved.is_null());

    assert_eq!(fx.set_parameter(GAIN, 0.5), noErr);
    assert_eq!(fx.set_parameter(MODE, 0.0), noErr);
    assert_eq!(
        fx.set(kAudioUnitProperty_ClassInfo, global, 0, &saved),
        noErr
    );
    assert_eq!(fx.parameter(GAIN), Ok(3.0));
    assert_eq!(fx.parameter(MODE), Ok(1.0));

    // In another instance too.
    let other = Host::new::<Fx>();
    assert_eq!(
        other.set(kAudioUnitProperty_ClassInfo, global, 0, &saved),
        noErr
    );
    assert_eq!(other.parameter(GAIN), Ok(3.0));

    // Another unit's state is refused, and changes nothing.
    let keys = Host::new::<Keys>();
    assert_eq!(
        keys.set(kAudioUnitProperty_ClassInfo, global, 0, &saved),
        kAudioUnitErr_InvalidPropertyValue
    );
    assert_eq!(keys.parameter(ParamId::from_name("level")), Ok(0.5));
    // SAFETY: the host releases what it was given.
    unsafe { CFRelease(saved) };

    // Not a dictionary, and a dictionary without data.
    let not_a_dictionary: CFPropertyListRef = ptr::null();
    assert_eq!(
        fx.set(kAudioUnitProperty_ClassInfo, global, 0, &not_a_dictionary),
        kAudioUnitErr_InvalidPropertyValue
    );
    // SAFETY: CoreFoundation's own functions, used as documented.
    unsafe {
        let empty = CFDictionaryCreateMutable(
            ptr::null(),
            0,
            type_dictionary_key_callbacks(),
            type_dictionary_value_callbacks(),
        );
        let empty: CFPropertyListRef = empty.cast_const();
        assert_eq!(
            fx.set(kAudioUnitProperty_ClassInfo, global, 0, &empty),
            kAudioUnitErr_InvalidPropertyValue
        );
        CFRelease(empty);
    }
    assert_eq!(fx.parameter(GAIN), Ok(3.0));
}

/// `Fx` again, with the id it works out when loaded in place of its
/// compiled one: a copy of the same library with another identity.
struct FxCopy(Fx);

impl Plugin for FxCopy {
    const INFO: PluginInfo = Fx::INFO;
    const LAYOUT: AudioLayout = Fx::LAYOUT;
    type Params = FxParams;

    fn info() -> PluginInfo {
        PluginInfo {
            id: "org.manguebyte.test.fx-copy",
            name: "Fx copy",
            ..Self::INFO
        }
    }
    fn new() -> Self {
        Self(Fx::new())
    }
    fn params(&self) -> Arc<FxParams> {
        self.0.params()
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.0.prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.0.process(buffer, context);
    }
}

/// The code a unit's state dictionary gives under `key`.
///
/// # Safety
///
/// `saved` must be a live dictionary.
unsafe fn code_in(saved: CFPropertyListRef, key: &str) -> u32 {
    // SAFETY: CoreFoundation's functions, on the caller's dictionary and a
    // string made and released here.
    unsafe {
        let key = CFStringCreateWithBytes(
            ptr::null(),
            key.as_ptr(),
            key.len() as _,
            kCFStringEncodingUTF8,
            0,
        );
        let value = CFDictionaryGetValue(saved, key);
        let mut code = 0i32;
        assert_ne!(
            CFNumberGetValue(value, kCFNumberSInt32Type, (&raw mut code).cast()),
            0
        );
        CFRelease(key);
        code as u32
    }
}

/// The codes, the `Info.plist` and the state follow the identity the
/// plugin works out at run time, so two copies of one library are two
/// units that refuse each other's state.
#[test]
fn the_codes_come_from_the_run_time_identity() {
    let _serial = serial();
    let id = "org.manguebyte.test.fx-copy";
    let plist = manguebyte_au::__private::info_plist::<FxCopy>("Fx", "org.example.fx", "1.0.0");
    let subtype = manguebyte_au::code_text(subtype_code(id));
    let manufacturer = manguebyte_au::code_text(manufacturer_code(id));
    assert!(plist.contains(&format!("<string>{subtype}</string>")));
    assert!(plist.contains(&format!("<string>{manufacturer}</string>")));
    assert!(plist.contains("Fx copy"), "{plist}");
    assert_ne!(subtype_code(id), subtype_code(Fx::INFO.id));

    let global = kAudioUnitScope_Global;
    let copy = Host::new::<FxCopy>();
    let fx = Host::new::<Fx>();
    let from_copy = copy
        .get::<CFPropertyListRef>(kAudioUnitProperty_ClassInfo, global, 0)
        .unwrap();
    let from_fx = fx
        .get::<CFPropertyListRef>(kAudioUnitProperty_ClassInfo, global, 0)
        .unwrap();
    // SAFETY: the dictionaries the units made, released once here.
    unsafe {
        assert_eq!(code_in(from_copy, "subtype"), as_os_type(subtype_code(id)));
        assert_eq!(
            code_in(from_copy, "manufacturer"),
            as_os_type(manufacturer_code(id))
        );
        assert_eq!(
            fx.set(kAudioUnitProperty_ClassInfo, global, 0, &from_copy),
            kAudioUnitErr_InvalidPropertyValue
        );
        assert_eq!(
            copy.set(kAudioUnitProperty_ClassInfo, global, 0, &from_fx),
            kAudioUnitErr_InvalidPropertyValue
        );
        assert_eq!(
            copy.set(kAudioUnitProperty_ClassInfo, global, 0, &from_copy),
            noErr
        );
        CFRelease(from_copy);
        CFRelease(from_fx);
    }
}

#[test]
fn the_codes_are_the_ones_the_plist_names() {
    let _serial = serial();
    let plist = manguebyte_au::info_plist(&Fx::INFO, "Fx", "org.manguebyte.test.fx", "1.2.3");
    let subtype = manguebyte_au::code_text(subtype_code(Fx::INFO.id));
    let manufacturer = manguebyte_au::code_text(manufacturer_code(Fx::INFO.id));
    assert!(plist.contains(&format!("<string>{subtype}</string>")));
    assert!(plist.contains(&format!("<string>{manufacturer}</string>")));
    assert!(plist.contains("<integer>66051</integer>"));
    // The state's dictionary carries the same codes.
    let fx = Host::new::<Fx>();
    let saved = fx
        .get::<CFPropertyListRef>(kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0)
        .unwrap();
    // SAFETY: the dictionary the unit made, and CoreFoundation's functions.
    unsafe {
        let key = CFStringCreateWithBytes(
            ptr::null(),
            b"subtype".as_ptr(),
            7,
            kCFStringEncodingUTF8,
            0,
        );
        let value = CFDictionaryGetValue(saved, key);
        let mut code = 0i32;
        assert_ne!(
            CFNumberGetValue(value, kCFNumberSInt32Type, (&raw mut code).cast()),
            0
        );
        assert_eq!(code as u32, as_os_type(subtype_code(Fx::INFO.id)));
        CFRelease(key);
        CFRelease(saved);
    }
}

#[test]
fn start_note_and_stop_note_play_as_midi_does() {
    let _serial = serial();
    type StartNote = unsafe extern "C" fn(
        *mut c_void,
        u32,
        u32,
        *mut u32,
        u32,
        *const MusicDeviceNoteParams,
    ) -> OSStatus;
    type StopNote = unsafe extern "C" fn(*mut c_void, u32, u32, u32) -> OSStatus;
    let keys = Host::new::<Keys>();
    assert!(
        Host::new::<Fx>()
            .lookup(kMusicDeviceStartNoteSelect)
            .is_null()
    );
    assert_eq!(keys.simple(kAudioUnitInitializeSelect), noErr);
    let start: StartNote = keys.method(kMusicDeviceStartNoteSelect);
    let stop: StopNote = keys.method(kMusicDeviceStopNoteSelect);
    let params = MusicDeviceNoteParams {
        argCount: 2,
        mPitch: 64.2,
        mVelocity: 127.0,
    };
    let mut note = 0u32;
    // SAFETY: the methods, with the note's parameters and a place for its id.
    let (started, stopped) = unsafe {
        (
            start(keys.this, 0, 3, &mut note, 100, &params),
            stop(keys.this, 3, note, 200),
        )
    };
    assert_eq!((started, stopped), (noErr, noErr));
    let mut output = AudioBuffer::<f32>::new(2, 256);
    assert_eq!(keys.render(256, &mut output, 0, false), noErr);
    let mut harness = Harness::<Keys>::new(44_100.0, 256);
    for (frame, bytes) in [(100, [0x93, 64, 127]), (200, [0x83, 64, 0])] {
        let message = MidiMessage::from_bytes(&bytes).unwrap();
        harness.schedule(frame, Event::from_midi(0, message).kind);
    }
    assert_same_bits(&output, &harness.render(256), "a note started and stopped");
    assert!(output.channel(0)[150] > 0.0 && output.channel(0)[250] == 0.0);
}

/// The text of a CoreFoundation string.
fn text_of(string: CFStringRef) -> String {
    let mut buffer = [0 as std::ffi::c_char; 256];
    // SAFETY: a live string, and a buffer of that size.
    let done =
        unsafe { CFStringGetCString(string, buffer.as_mut_ptr(), 256, kCFStringEncodingUTF8) };
    assert_ne!(done, 0, "not a string");
    // SAFETY: the buffer ends with a NUL.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned()
}

/// A new CoreFoundation string, which the caller releases.
fn new_string(text: &str) -> CFStringRef {
    // SAFETY: the text's own bytes, in UTF-8.
    unsafe {
        CFStringCreateWithBytes(
            ptr::null(),
            text.as_ptr(),
            text.len() as CFIndex,
            kCFStringEncodingUTF8,
            0,
        )
    }
}

#[test]
fn the_present_preset_is_kept_and_travels_with_the_state() {
    let _serial = serial();
    let global = kAudioUnitScope_Global;
    let fx = Host::new::<Fx>();
    for property in [
        kAudioUnitProperty_PresentPreset,
        kAudioUnitProperty_CurrentPreset,
    ] {
        assert_eq!(fx.info(property, global, 0), Ok((16, true)));
    }

    // Untitled, a user's preset, until the host says otherwise. The present
    // preset's name is the caller's to release; the current preset's, the
    // deprecated property, stays the unit's.
    let preset = fx
        .get::<AUPreset>(kAudioUnitProperty_PresentPreset, global, 0)
        .unwrap();
    assert_eq!(
        (preset.presetNumber, text_of(preset.presetName)),
        (-1, "Untitled".into())
    );
    // SAFETY: the reference the unit handed over.
    unsafe { CFRelease(preset.presetName) };
    let current = fx
        .get::<AUPreset>(kAudioUnitProperty_CurrentPreset, global, 0)
        .unwrap();
    assert_eq!(text_of(current.presetName), "Untitled");

    // Set, then got back; the host keeps its own string.
    HEARD.lock().unwrap().clear();
    let add: AddListener = fx.method(kAudioUnitAddPropertyListenerSelect);
    let user = 9 as *mut c_void;
    // SAFETY: the method, with a listener that outlives the unit.
    let status = unsafe {
        add(
            fx.this,
            kAudioUnitProperty_PresentPreset,
            Some(listener),
            user,
        )
    };
    assert_eq!(status, noErr);
    let bright = new_string("Bright");
    let given = AUPreset {
        presetNumber: 5,
        presetName: bright,
    };
    assert_eq!(
        fx.set(kAudioUnitProperty_PresentPreset, global, 0, &given),
        noErr
    );
    // SAFETY: the test's own reference.
    unsafe { CFRelease(bright) };
    let preset = fx
        .get::<AUPreset>(kAudioUnitProperty_PresentPreset, global, 0)
        .unwrap();
    assert_eq!(
        (preset.presetNumber, text_of(preset.presetName)),
        (5, "Bright".into())
    );
    // SAFETY: as above.
    unsafe { CFRelease(preset.presetName) };
    assert_eq!(
        *HEARD.lock().unwrap(),
        [(kAudioUnitProperty_PresentPreset, global, 0, 9)]
    );
    // A preset with no name is untitled.
    let nameless = AUPreset {
        presetNumber: 2,
        presetName: ptr::null(),
    };
    assert_eq!(
        fx.set(kAudioUnitProperty_CurrentPreset, global, 0, &nameless),
        noErr
    );
    let current = fx
        .get::<AUPreset>(kAudioUnitProperty_CurrentPreset, global, 0)
        .unwrap();
    assert_eq!(
        (current.presetNumber, text_of(current.presetName)),
        (2, "Untitled".into())
    );

    // The state carries the name under "name", and a unit given the state
    // takes it as its present preset, a user's.
    let warm = new_string("Warm");
    let given = AUPreset {
        presetNumber: 0,
        presetName: warm,
    };
    assert_eq!(
        fx.set(kAudioUnitProperty_PresentPreset, global, 0, &given),
        noErr
    );
    // SAFETY: the test's own reference.
    unsafe { CFRelease(warm) };
    let saved = fx
        .get::<CFPropertyListRef>(kAudioUnitProperty_ClassInfo, global, 0)
        .unwrap();
    // SAFETY: the dictionary the unit made, and CoreFoundation's functions.
    unsafe {
        let key = new_string("name");
        assert_eq!(text_of(CFDictionaryGetValue(saved, key)), "Warm");
        CFRelease(key);
    }
    let other = Host::new::<Fx>();
    assert_eq!(
        other.set(kAudioUnitProperty_ClassInfo, global, 0, &saved),
        noErr
    );
    let preset = other
        .get::<AUPreset>(kAudioUnitProperty_PresentPreset, global, 0)
        .unwrap();
    assert_eq!(
        (preset.presetNumber, text_of(preset.presetName)),
        (-1, "Warm".into())
    );
    // SAFETY: the references the unit handed over.
    unsafe {
        CFRelease(preset.presetName);
        CFRelease(saved);
    }
}

// --- MIDI output ---------------------------------------------------------------

/// Plays the same phrase every render, and sends it out: a note on frame
/// 2, its pressure on 5, its end on 7, a controller on 9, and a note sent
/// past the end of the render.
struct Phrase {
    params: Arc<KeysParams>,
}

impl Plugin for Phrase {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.phrase",
        name: "Phrase",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays a phrase",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    const NOTE_OUTPUT: bool = true;
    type Params = KeysParams;

    fn new() -> Self {
        Self {
            params: Keys::new().params(),
        }
    }
    fn params(&self) -> Arc<KeysParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.process_with_output(buffer, context, &mut OutputBuffer::new());
    }
    fn process_with_output(
        &mut self,
        _: &mut AudioBuffer<f32>,
        _: &ProcessContext<'_, f32>,
        output: &mut OutputBuffer,
    ) {
        let note = NoteRef::new(3, 60);
        output.note_on(2, note, 100.0 / 127.0);
        output.expression(5, note, Expression::Pressure, 32.0 / 127.0);
        output.note_off(7, note, 0.0);
        output.midi1(
            9,
            MidiMessage::ControlChange {
                channel: 3,
                controller: 1,
                value: 64,
            },
        );
        output.note_on(40, NoteRef::new(0, 72), 1.0);
    }
}

/// One MIDI message the host was handed, with its time stamp.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Received {
    time: u64,
    length: usize,
    bytes: [u8; 3],
}

/// What the MIDI output callback was handed, with room set aside so that
/// keeping it does not use the heap inside the render.
static RECEIVED: Mutex<Vec<Received>> = Mutex::new(Vec::new());
/// How many lists the callback was handed.
static LISTS: Mutex<usize> = Mutex::new(0);

/// Reads a `MIDIPacketList` as CoreMIDI lays it out.
unsafe extern "C" fn midi_out(
    user: *mut c_void,
    _timestamp: *const AudioTimeStamp,
    output: u32,
    list: *const c_void,
) -> OSStatus {
    assert_eq!((user as usize, output), (9, 0));
    let mut received = RECEIVED.lock().unwrap();
    *LISTS.lock().unwrap() += 1;
    // SAFETY: the unit hands a list of as many packets as its count says,
    // valid for the call.
    unsafe {
        let bytes = list.cast::<u8>();
        let count = bytes.cast::<u32>().read_unaligned();
        let mut at = MIDI_PACKET_LIST_PACKETS;
        for _ in 0..count {
            let time = bytes.add(at).cast::<u64>().read_unaligned();
            let length = bytes
                .add(at + MIDI_PACKET_LENGTH)
                .cast::<u16>()
                .read_unaligned() as usize;
            assert!(length <= 3, "one message to a packet");
            let mut message = [0u8; 3];
            ptr::copy_nonoverlapping(
                bytes.add(at + MIDI_PACKET_DATA),
                message.as_mut_ptr(),
                length,
            );
            received.push(Received {
                time,
                length,
                bytes: message,
            });
            at = (at + MIDI_PACKET_DATA + length).next_multiple_of(MIDI_PACKET_ALIGN);
        }
    }
    noErr
}

#[test]
fn midi_the_plugin_sends_reaches_the_host_and_reads_back_as_it_was_sent() {
    let _serial = serial();
    let global = kAudioUnitScope_Global;
    let phrase = Host::new::<Phrase>();
    // One MIDI output, named.
    let names = phrase
        .get::<CFArrayRef>(kAudioUnitProperty_MIDIOutputCallbackInfo, global, 0)
        .unwrap();
    assert!(!names.is_null());
    #[cfg(not(target_os = "macos"))]
    {
        // SAFETY: an array the unit made.
        let values = unsafe { manguebyte_au::fake_cf::array_values(names) };
        assert_eq!(values.len(), 1);
        assert_eq!(text_of(values[0]), "MIDI Out");
    }
    // SAFETY: the host releases what it was given.
    unsafe { CFRelease(names) };
    assert_eq!(
        phrase.info(kAudioUnitProperty_MIDIOutputCallback, global, 0),
        Ok((size_of::<AUMIDIOutputCallbackStruct>() as u32, true))
    );
    let callback = AUMIDIOutputCallbackStruct {
        midiOutputCallback: Some(midi_out),
        userData: 9 as *mut c_void,
    };
    assert_eq!(
        phrase.set(kAudioUnitProperty_MIDIOutputCallback, global, 0, &callback),
        noErr
    );
    assert_eq!(phrase.simple(kAudioUnitInitializeSelect), noErr);

    RECEIVED.lock().unwrap().clear();
    RECEIVED.lock().unwrap().reserve(64);
    *LISTS.lock().unwrap() = 0;
    let mut output = AudioBuffer::<f32>::new(2, 16);
    assert_eq!(phrase.render(16, &mut output, 0, false), noErr);
    let received = RECEIVED.lock().unwrap().clone();
    assert_eq!(*LISTS.lock().unwrap(), 1);
    let times: Vec<u64> = received.iter().map(|message| message.time).collect();
    // The note sent past the end of the render is on its last frame.
    assert_eq!(times, [2, 5, 7, 9, 15]);

    // Read back as any wrapper reads MIDI, it is what the plugin sent.
    let mut input = NoteInput::new(Mpe::Off);
    let mut events = Vec::new();
    for message in &received {
        let message = MidiMessage::from_bytes(&message.bytes[..message.length]).unwrap();
        input.midi1(0, message, |event: Event| events.push(event.kind));
    }
    let note = NoteRef::new(3, 60);
    assert_eq!(
        events,
        [
            EventKind::NoteOn {
                note,
                velocity: 100.0 / 127.0
            },
            EventKind::NoteExpression {
                note,
                expression: Expression::Pressure,
                value: 32.0 / 127.0
            },
            EventKind::NoteOff {
                note,
                velocity: 0.0
            },
            EventKind::Midi(MidiMessage::ControlChange {
                channel: 3,
                controller: 1,
                value: 64
            }),
            EventKind::NoteOn {
                note: NoteRef::new(0, 72),
                velocity: 1.0
            },
        ]
    );

    // With the callback taken away, nothing is handed over.
    let none = AUMIDIOutputCallbackStruct {
        midiOutputCallback: None,
        userData: ptr::null_mut(),
    };
    assert_eq!(
        phrase.set(kAudioUnitProperty_MIDIOutputCallback, global, 0, &none),
        noErr
    );
    RECEIVED.lock().unwrap().clear();
    assert_eq!(phrase.render(16, &mut output, 0, false), noErr);
    assert!(RECEIVED.lock().unwrap().is_empty());

    // A unit that sends nothing has no MIDI output.
    let keys = Host::new::<Keys>();
    assert_eq!(
        keys.info(kAudioUnitProperty_MIDIOutputCallbackInfo, global, 0),
        Err(kAudioUnitErr_InvalidProperty)
    );
}

// --- Channel counts ---------------------------------------------------------------

/// The layouts `Shape` was given, in order.
static SHAPE_LAYOUTS: Mutex<Vec<AudioLayout>> = Mutex::new(Vec::new());

/// Halves its input, and runs in stereo or in mono, keyed either way.
struct Shape {
    params: Arc<KeysParams>,
}

impl Plugin for Shape {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.shape",
        name: "Shape",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Runs in several shapes",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT.with_sidechain(2);
    const LAYOUTS: &'static [AudioLayout] = &[
        AudioLayout::STEREO_EFFECT.with_sidechain(2),
        AudioLayout::MONO_EFFECT.with_sidechain(1),
    ];
    type Params = KeysParams;

    fn new() -> Self {
        Self {
            params: Keys::new().params(),
        }
    }
    fn params(&self) -> Arc<KeysParams> {
        Arc::clone(&self.params)
    }
    fn set_layout(&mut self, layout: AudioLayout) {
        SHAPE_LAYOUTS.lock().unwrap().push(layout);
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
        buffer.apply_gain(0.5);
    }
}

#[test]
fn channel_counts_follow_the_layouts_offered() {
    let _serial = serial();
    SHAPE_LAYOUTS.lock().unwrap().clear();
    let (global, input, output) = (
        kAudioUnitScope_Global,
        kAudioUnitScope_Input,
        kAudioUnitScope_Output,
    );
    let shape = Host::new::<Shape>();
    assert_eq!(
        shape.info(kAudioUnitProperty_SupportedNumChannels, global, 0),
        Ok((8, false))
    );
    let mut infos = [AUChannelInfo {
        inChannels: 0,
        outChannels: 0,
    }; 2];
    let mut size = 8u32;
    assert_eq!(
        shape.get_raw(
            kAudioUnitProperty_SupportedNumChannels,
            global,
            0,
            infos.as_mut_ptr().cast(),
            &mut size
        ),
        noErr
    );
    assert_eq!(
        infos,
        [
            AUChannelInfo {
                inChannels: 2,
                outChannels: 2
            },
            AUChannelInfo {
                inChannels: 1,
                outChannels: 1
            }
        ]
    );
    // Each element offers its widths' layouts.
    let mut tags = [0u32; 2];
    let mut size = 8u32;
    assert_eq!(
        shape.get_raw(
            kAudioUnitProperty_SupportedChannelLayoutTags,
            input,
            1,
            tags.as_mut_ptr().cast(),
            &mut size
        ),
        noErr
    );
    assert_eq!(
        tags,
        [kAudioChannelLayoutTag_Stereo, kAudioChannelLayoutTag_Mono]
    );

    let stereo = shape
        .get::<AudioStreamBasicDescription>(kAudioUnitProperty_StreamFormat, output, 0)
        .unwrap();
    let mono = AudioStreamBasicDescription {
        mChannelsPerFrame: 1,
        ..stereo
    };
    let three = AudioStreamBasicDescription {
        mChannelsPerFrame: 3,
        ..stereo
    };
    assert_eq!(
        shape.set(kAudioUnitProperty_StreamFormat, input, 0, &three),
        kAudioUnitErr_FormatNotSupported
    );
    // Mono in and out with a stereo sidechain is no layout of the plugin's.
    assert_eq!(
        shape.set(kAudioUnitProperty_StreamFormat, input, 0, &mono),
        noErr
    );
    assert_eq!(
        shape.set(kAudioUnitProperty_StreamFormat, output, 0, &mono),
        noErr
    );
    assert_eq!(
        shape.simple(kAudioUnitInitializeSelect),
        kAudioUnitErr_FormatNotSupported
    );
    assert_eq!(
        shape.set(kAudioUnitProperty_StreamFormat, input, 1, &mono),
        noErr
    );
    assert_eq!(
        shape.get::<AudioStreamBasicDescription>(kAudioUnitProperty_StreamFormat, input, 1),
        Ok(mono)
    );
    assert_eq!(shape.simple(kAudioUnitInitializeSelect), noErr);
    assert_eq!(
        SHAPE_LAYOUTS.lock().unwrap().last(),
        Some(&AudioLayout::MONO_EFFECT.with_sidechain(1))
    );
    // Not while initialised.
    assert_eq!(
        shape.set(kAudioUnitProperty_StreamFormat, output, 0, &stereo),
        kAudioUnitErr_Initialized
    );

    let mut feed = Feed {
        audio: signal(1, 64, 0.1),
        position: 0,
    };
    assert_eq!(shape.feed(0, &mut feed), noErr);
    let mut out = AudioBuffer::<f32>::new(1, 32);
    assert_eq!(shape.render(32, &mut out, 0, false), noErr);
    let mut expected = signal(1, 32, 0.1);
    expected.apply_gain(0.5);
    assert_same_bits(&out, &expected, "mono");
}

// --- Parameters renamed while the unit runs ------------------------------------

struct ChoirParams {
    upper: BoolParam,
    swell: FloatParam,
}
param_set!(ChoirParams { upper, swell });

const UPPER: ParamId = ParamId::from_name("upper");
const SWELL: ParamId = ParamId::from_name("swell");

static CHOIR_PARAMS: Mutex<Option<Arc<ChoirParams>>> = Mutex::new(None);

/// Passes its input through; it is here for its parameters, which the
/// test renames as a plugin that loads another instrument does.
struct Choir {
    params: Arc<ChoirParams>,
}

impl Plugin for Choir {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.choir",
        name: "Choir",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Renames its parameters",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = ChoirParams;

    fn new() -> Self {
        let params = Arc::new(ChoirParams {
            upper: BoolParam::new("upper", "Upper", false),
            swell: FloatParam::new("swell", "Swell", FloatRange::linear(0.0, 1.0), 0.5),
        });
        *CHOIR_PARAMS.lock().unwrap() = Some(Arc::clone(&params));
        Self { params }
    }
    fn params(&self) -> Arc<ChoirParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

/// The name a unit gives in a parameter's information, and its unit; the
/// name is checked to be the same in both its fields.
fn described(unit: &Host, param: ParamId) -> (String, u32) {
    let info = unit
        .get::<AudioUnitParameterInfo>(
            kAudioUnitProperty_ParameterInfo,
            kAudioUnitScope_Global,
            param.0,
        )
        .unwrap();
    // SAFETY: the name field ends with a NUL.
    let name = unsafe { CStr::from_ptr(info.name.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(text_of(info.cfNameString), name);
    // SAFETY: the host releases the name, as the flags say.
    unsafe { CFRelease(info.cfNameString) };
    (name, info.unit)
}

/// Renames made while the unit is not initialized are announced once, as
/// a change of the parameter list, when it is, and renames made as state
/// is restored are announced with it; the parameters' information gives
/// the new names and unit throughout.
#[test]
fn renamed_parameters_are_announced_once_and_described_by_their_new_names() {
    let _serial = serial();
    HEARD.lock().unwrap().clear();
    let choir = Host::new::<Choir>();
    let global = kAudioUnitScope_Global;
    let add: AddListener = choir.method(kAudioUnitAddPropertyListenerSelect);
    let user = 7 as *mut c_void;
    // SAFETY: the method, with a listener that outlives the unit.
    let status = unsafe {
        add(
            choir.this,
            kAudioUnitProperty_ParameterList,
            Some(listener),
            user,
        )
    };
    assert_eq!(status, noErr);
    assert_eq!(
        described(&choir, SWELL),
        ("Swell".into(), kAudioUnitParameterUnit_Generic)
    );

    let params = CHOIR_PARAMS.lock().unwrap().clone().unwrap();
    let renamer = Arc::clone(&params);
    std::thread::spawn(move || {
        renamer.upper.info().set_display_name("Soprano");
        renamer.swell.info().set_display_name("Expression");
        renamer.swell.info().set_display_unit("dB");
    })
    .join()
    .unwrap();
    // Asked for before anything announces it, the information is new.
    assert_eq!(described(&choir, UPPER).0, "Soprano");
    assert_eq!(choir.simple(kAudioUnitInitializeSelect), noErr);
    assert_eq!(
        *HEARD.lock().unwrap(),
        [(kAudioUnitProperty_ParameterList, global, 0, 7)]
    );
    assert_eq!(
        described(&choir, SWELL),
        ("Expression".into(), kAudioUnitParameterUnit_Decibels)
    );
    // Nothing renamed since: nothing more to hear.
    assert_eq!(choir.simple(kAudioUnitUninitializeSelect), noErr);
    assert_eq!(choir.simple(kAudioUnitInitializeSelect), noErr);
    assert_eq!(HEARD.lock().unwrap().len(), 1);

    // A state restored with another instrument's names.
    let saved = choir
        .get::<CFPropertyListRef>(kAudioUnitProperty_ClassInfo, global, 0)
        .unwrap();
    params.upper.info().set_display_name("Treble");
    assert_eq!(
        choir.set(kAudioUnitProperty_ClassInfo, global, 0, &saved),
        noErr
    );
    // SAFETY: the host releases what it was given.
    unsafe { CFRelease(saved) };
    assert_eq!(
        HEARD.lock().unwrap().as_slice(),
        [(kAudioUnitProperty_ParameterList, global, 0, 7); 2]
    );
    assert_eq!(described(&choir, UPPER).0, "Treble");
    assert_eq!(choir.simple(kAudioUnitUninitializeSelect), noErr);

    let remove: RemoveListener = choir.method(kAudioUnitRemovePropertyListenerSelect);
    // SAFETY: the method.
    let status = unsafe { remove(choir.this, kAudioUnitProperty_ParameterList, Some(listener)) };
    assert_eq!(status, noErr);
}
