//! Drives the VST 3 wrapper through its binary interface, the way a host
//! does.
//!
//! Nothing here reaches into the wrapper's internals. The test plays the
//! part of a host: it asks for the factory, creates the component, queries
//! its interfaces and calls `process` with raw buffers, parameter queues and
//! event lists that it implements itself as COM objects. If these pass, a
//! real host sees the same behaviour.
//!
//! Every `unsafe` block in this file is a call across that interface with
//! pointers the test owns and keeps alive for the duration of the call, so
//! the blocks are not commented one by one.
#![allow(clippy::undocumented_unsafe_blocks)]
#![allow(non_snake_case)]
// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast needed on one platform is redundant on the other.
#![allow(clippy::unnecessary_cast)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::{assert_eq, ptr};

use manguebyte_core::{AtomicF64, AudioBuffer, NoDenormals, ParamId};
use manguebyte_dsp::event::{EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth, Voice, split_at_events};
use manguebyte_midi::MidiMessage;
use manguebyte_params::{BoolParam, ChoiceParam, FloatParam, FloatRange, param_set};
use manguebyte_plugin::{
    AudioLayout, MidiController, MidiControllers, Plugin, PluginInfo, PluginKind,
};
use manguebyte_vst3::vst3::Steinberg::Vst::{
    self as vst, IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentTrait,
    IEditController, IEditControllerTrait, IEventList, IEventListTrait, IMidiMapping,
    IMidiMappingTrait, INoteExpressionController, INoteExpressionControllerTrait, IParamValueQueue,
    IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait, IProcessContextRequirements,
    IProcessContextRequirementsTrait, IUnitInfo, IUnitInfoTrait,
};
use manguebyte_vst3::vst3::Steinberg::{
    self as sb, IBStream, IBStreamTrait, IPluginBaseTrait, IPluginFactory, IPluginFactory2,
    IPluginFactory2Trait, IPluginFactory3, IPluginFactory3Trait, IPluginFactoryTrait,
    kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, tresult,
};
use manguebyte_vst3::vst3::{Class, ComPtr, ComWrapper, Interface};
use manguebyte_vst3::{class_id, param_id, plugin_factory};

// --- Watching the heap ---------------------------------------------------

thread_local! {
    static IN_PROCESS: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
        let _ = IN_PROCESS.try_with(|watching| {
            if watching.get() {
                let _ = HEAP_CALLS.try_with(|calls| calls.set(calls.get() + 1));
            }
        });
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        Self::record();
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

// --- The plugins under test ---------------------------------------------

struct LevelParams {
    level: FloatParam,
    shape: ChoiceParam,
    explode: BoolParam,
}
param_set!(LevelParams {
    level,
    shape,
    explode
});

const LEVEL: ParamId = ParamId::from_name("level");
const SHAPE: ParamId = ParamId::from_name("shape");

/// Where the test finds the parameters of the most recently created
/// `Level`, to switch its failure mode on.
static LEVEL_PARAMS: Mutex<Option<Arc<LevelParams>>> = Mutex::new(None);
static TEMPO_SEEN: Mutex<Option<Arc<AtomicF64>>> = Mutex::new(None);

/// Half of the smallest normal number, worked out by the processor and not
/// by the compiler. It is subnormal, unless such numbers are being flushed.
fn half_of_the_smallest() -> f32 {
    use std::hint::black_box;
    black_box(black_box(f32::MIN_POSITIVE) * black_box(0.5))
}

/// What `half_of_the_smallest` came to inside the last `Level::process`.
static HALF_INSIDE_PROCESS: AtomicU32 = AtomicU32::new(u32::MAX);

fn level_params() -> Arc<LevelParams> {
    LEVEL_PARAMS.lock().unwrap().clone().unwrap()
}

/// Scales its input by `level`, following events alone and never polling.
struct Level {
    params: Arc<LevelParams>,
    current: f32,
    tempo_seen: Arc<AtomicF64>,
}

impl Plugin for Level {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.level",
        name: "Level",
        vendor: "Manguebyte",
        version: "1.2.3",
        description: "Scales the input",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = LevelParams;

    fn new() -> Self {
        let params = Arc::new(LevelParams {
            level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 4.0), 1.0)
                .with_unit("x"),
            shape: ChoiceParam::new("shape", "Shape", &["Sine", "Saw", "Square"], 1),
            explode: BoolParam::new("explode", "Explode", false).hidden(),
        });
        *LEVEL_PARAMS.lock().unwrap() = Some(Arc::clone(&params));
        let tempo_seen = Arc::new(AtomicF64::new(-1.0));
        *TEMPO_SEEN.lock().unwrap() = Some(Arc::clone(&tempo_seen));
        Self {
            current: params.level.value(),
            params,
            tempo_seen,
        }
    }

    fn params(&self) -> Arc<LevelParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, _: &ProcessSpec) {}

    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        assert!(!self.params.explode.value(), "the plugin was told to fail");
        HALF_INSIDE_PROCESS.store(half_of_the_smallest().to_bits(), Ordering::Relaxed);
        self.tempo_seen.store(
            context.transport.tempo_bpm.unwrap_or(-1.0),
            Ordering::Relaxed,
        );
        split_at_events(
            &mut self.current,
            buffer,
            context.events,
            |current, event| {
                if let EventKind::ParamValue {
                    param: LEVEL,
                    value,
                } = event.kind
                {
                    *current = value as f32;
                }
            },
            |current, piece| piece.apply_gain(*current),
        );
    }

    fn latency_samples(&self) -> usize {
        7
    }

    fn tail_samples(&self) -> manguebyte_plugin::Tail {
        manguebyte_plugin::Tail::Samples(480)
    }
}

// The macro is what a real plugin crate uses, so it is expanded here too.
manguebyte_vst3::export_vst3!(Level);

/// Outputs its key number, moved by bend and tuning and scaled by volume,
/// while its key is down.
#[derive(Default)]
struct KeyVoice {
    key: f32,
    volume: f32,
    bend: f32,
    tuning: f32,
}

impl Voice for KeyVoice {
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn note_on(&mut self, note: NoteRef, _: f32) {
        self.key = f32::from(note.key);
    }
    fn note_off(&mut self, _: f32) {
        self.key = 0.0;
    }
    fn choke(&mut self) {
        self.key = 0.0;
        self.volume = 1.0;
        self.tuning = 0.0;
    }
    fn is_active(&self) -> bool {
        self.key > 0.0
    }
    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        if self.key == 0.0 {
            return;
        }
        let sample = (self.key + self.bend + self.tuning) * self.volume;
        for channel in buffer.iter_channels_mut() {
            channel.iter_mut().for_each(|out| *out += sample);
        }
    }
    fn expression(&mut self, expression: Expression, value: f64) {
        match expression {
            Expression::Volume => self.volume = value as f32,
            Expression::Tuning => self.tuning = value as f32,
            // Pressure is added to the key so the test can see it arrive.
            Expression::Pressure => self.tuning = 100.0 * value as f32,
            _ => {}
        }
    }
    fn pitch_bend(&mut self, semitones: f64) {
        self.bend = semitones as f32;
    }
}

struct KeysParams {
    brightness: FloatParam,
}
param_set!(KeysParams { brightness });

struct Keys {
    synth: Synth<KeyVoice>,
    params: Arc<KeysParams>,
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
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 0,
        outputs: 1,
        sidechain: 2,
    };
    type Params = KeysParams;

    fn new() -> Self {
        Self {
            synth: Synth::with_voices(4, KeyVoice::default),
            params: Arc::new(KeysParams {
                brightness: FloatParam::new(
                    "brightness",
                    "Brightness",
                    FloatRange::linear(0.0, 100.0),
                    50.0,
                ),
            }),
        }
    }
    fn params(&self) -> Arc<KeysParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.synth.prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.synth.process_with(buffer, context);
        // Make the sidechain audible so the test can see it arrived.
        if let Some(sidechain) = context.sidechain {
            let level = sidechain.peak();
            buffer.channel_mut(0).iter_mut().for_each(|s| *s += level);
        }
    }
    fn reset(&mut self) {
        self.synth.reset();
    }
}

// --- Host-side objects ----------------------------------------------------

/// A stream in memory that moves only a few bytes per call, to exercise
/// the plugin's read and write loops.
struct MemoryStream {
    bytes: RefCell<Vec<u8>>,
    position: Cell<usize>,
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, count: i32, read: *mut i32) -> tresult {
        let bytes = self.bytes.borrow();
        let position = self.position.get();
        let take = (count.max(0) as usize).min(5).min(bytes.len() - position);
        unsafe {
            ptr::copy_nonoverlapping(bytes[position..].as_ptr(), buffer.cast::<u8>(), take);
            if !read.is_null() {
                *read = take as i32;
            }
        }
        self.position.set(position + take);
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, count: i32, written: *mut i32) -> tresult {
        let take = (count.max(0) as usize).min(7);
        let data = unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), take) };
        self.bytes.borrow_mut().extend_from_slice(data);
        if !written.is_null() {
            unsafe { *written = take as i32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, _position: i64, _mode: i32, _result: *mut i64) -> tresult {
        kNotImplemented
    }

    unsafe fn tell(&self, _position: *mut i64) -> tresult {
        kNotImplemented
    }
}

/// The automation points of one parameter in one block.
struct Queue {
    id: u32,
    points: Vec<(i32, f64)>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> u32 {
        self.id
    }
    unsafe fn getPointCount(&self) -> i32 {
        self.points.len() as i32
    }
    unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut f64) -> tresult {
        let Some(&(at, to)) = self.points.get(index as usize) else {
            return kResultFalse;
        };
        unsafe {
            *offset = at;
            *value = to;
        }
        kResultOk
    }
    unsafe fn addPoint(&self, _offset: i32, _value: f64, _index: *mut i32) -> tresult {
        kResultFalse
    }
}

struct Changes {
    queues: Vec<ComWrapper<Queue>>,
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> i32 {
        self.queues.len() as i32
    }
    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        self.queues
            .get(index as usize)
            .and_then(|queue| queue.as_com_ref::<IParamValueQueue>())
            .map_or(ptr::null_mut(), |queue| queue.as_ptr())
    }
    unsafe fn addParameterData(&self, _id: *const u32, _index: *mut i32) -> *mut IParamValueQueue {
        ptr::null_mut()
    }
}

struct Events {
    events: Vec<vst::Event>,
}

impl Class for Events {
    type Interfaces = (IEventList,);
}

impl IEventListTrait for Events {
    unsafe fn getEventCount(&self) -> i32 {
        self.events.len() as i32
    }
    unsafe fn getEvent(&self, index: i32, event: *mut vst::Event) -> tresult {
        let Some(found) = self.events.get(index as usize) else {
            return kResultFalse;
        };
        unsafe { *event = *found };
        kResultOk
    }
    unsafe fn addEvent(&self, _event: *mut vst::Event) -> tresult {
        kResultFalse
    }
}

fn event(offset: i32, kind: u32) -> vst::Event {
    let mut event: vst::Event = unsafe { std::mem::zeroed() };
    event.sampleOffset = offset;
    event.r#type = kind as u16;
    event
}

fn note_on(offset: i32, pitch: i16, note_id: i32, tuning: f32) -> vst::Event {
    let mut event = event(offset, vst::Event_::EventTypes_::kNoteOnEvent as u32);
    event.__field0.noteOn = vst::NoteOnEvent {
        channel: 0,
        pitch,
        tuning,
        velocity: 1.0,
        length: 0,
        noteId: note_id,
    };
    event
}

fn note_off(offset: i32, pitch: i16, note_id: i32) -> vst::Event {
    let mut event = event(offset, vst::Event_::EventTypes_::kNoteOffEvent as u32);
    event.__field0.noteOff = vst::NoteOffEvent {
        channel: 0,
        pitch,
        velocity: 0.0,
        noteId: note_id,
        tuning: 0.0,
    };
    event
}

/// What one call to `process` produced.
struct Processed {
    status: tresult,
    /// One vector per channel of the main bus.
    output: Vec<Vec<f32>>,
    /// How many times the plugin used the heap during the call.
    heap_calls: usize,
}

struct Host {
    factory: ComPtr<IPluginFactory>,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: ComPtr<IEditController>,
}

fn iid<I: Interface>() -> *const c_char {
    I::IID.as_ptr().cast()
}

impl Host {
    fn new<P: Plugin>(factory: *mut c_void) -> Self {
        unsafe {
            let factory = ComPtr::<IPluginFactory>::from_raw(factory.cast()).unwrap();
            let class = class_id(P::INFO.id);
            let mut object = ptr::null_mut();
            let result = factory.createInstance(class.as_ptr(), iid::<IComponent>(), &mut object);
            assert_eq!(
                result, kResultOk,
                "the factory refused to create the plugin"
            );
            let component = ComPtr::<IComponent>::from_raw(object.cast()).unwrap();
            assert_eq!(component.initialize(ptr::null_mut()), kResultOk);
            Self {
                factory,
                processor: component.cast().expect("no IAudioProcessor"),
                controller: component.cast().expect("no IEditController"),
                component,
            }
        }
    }

    fn activate(&self, sample_rate: f64, max_block: i32) {
        let mut setup = vst::ProcessSetup {
            processMode: vst::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: vst::SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: max_block,
            sampleRate: sample_rate,
        };
        unsafe {
            assert_eq!(self.processor.setupProcessing(&mut setup), kResultOk);
            assert_eq!(self.component.setActive(1), kResultOk);
            assert_eq!(self.processor.setProcessing(1), kResultOk);
        }
    }

    fn bus_count(&self, media: u32, direction: u32) -> i32 {
        unsafe { self.component.getBusCount(media as _, direction as _) }
    }

    fn bus_info(&self, media: u32, direction: u32, index: i32) -> Option<vst::BusInfo> {
        unsafe {
            let mut info: vst::BusInfo = std::mem::zeroed();
            (self
                .component
                .getBusInfo(media as _, direction as _, index, &mut info)
                == kResultOk)
                .then_some(info)
        }
    }

    fn param_info(&self, index: i32) -> Option<vst::ParameterInfo> {
        unsafe {
            let mut info: vst::ParameterInfo = std::mem::zeroed();
            (self.controller.getParameterInfo(index, &mut info) == kResultOk).then_some(info)
        }
    }

    fn param_text(&self, id: u32, value: f64) -> Option<String> {
        let mut text = [0u16; 128];
        unsafe {
            (self.controller.getParamStringByValue(id, value, &mut text) == kResultOk)
                .then(|| wide(&text))
        }
    }

    fn param_from_text(&self, id: u32, text: &str) -> Option<f64> {
        let mut units: Vec<u16> = text.encode_utf16().chain([0]).collect();
        let mut value = f64::NAN;
        unsafe {
            (self
                .controller
                .getParamValueByString(id, units.as_mut_ptr(), &mut value)
                == kResultOk)
                .then_some(value)
        }
    }

    fn save(&self) -> Option<Vec<u8>> {
        let stream = ComWrapper::new(MemoryStream {
            bytes: RefCell::new(Vec::new()),
            position: Cell::new(0),
        });
        let pointer = stream.as_com_ref::<IBStream>().unwrap().as_ptr();
        let result = unsafe { self.component.getState(pointer) };
        (result == kResultOk).then(|| stream.bytes.borrow().clone())
    }

    fn load(&self, bytes: &[u8]) -> tresult {
        let stream = ComWrapper::new(MemoryStream {
            bytes: RefCell::new(bytes.to_vec()),
            position: Cell::new(0),
        });
        let pointer = stream.as_com_ref::<IBStream>().unwrap().as_ptr();
        unsafe { self.component.setState(pointer) }
    }

    /// Processes one block.
    ///
    /// `main` is the main bus's memory, used for input and output alike, as
    /// a host does when processing in place. `sidechain`, if given, is a
    /// second input bus. `changes` holds automation points per parameter
    /// id, and `frames` may be zero for a block that only carries those.
    fn process(
        &self,
        main: &mut [Vec<f32>],
        frames: usize,
        sidechain: Option<&mut [Vec<f32>]>,
        changes: Vec<(u32, Vec<(i32, f64)>)>,
        events: Vec<vst::Event>,
        tempo: Option<f64>,
    ) -> Processed {
        let mut main_channels: Vec<*mut f32> = main.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let main_bus = vst::AudioBusBuffers {
            numChannels: main_channels.len() as i32,
            silenceFlags: 0,
            __field0: vst::AudioBusBuffers__type0 {
                channelBuffers32: main_channels.as_mut_ptr(),
            },
        };
        let mut side_channels: Vec<*mut f32> = sidechain
            .map(|channels| channels.iter_mut().map(|c| c.as_mut_ptr()).collect())
            .unwrap_or_default();
        let side_bus = vst::AudioBusBuffers {
            numChannels: side_channels.len() as i32,
            silenceFlags: 0,
            __field0: vst::AudioBusBuffers__type0 {
                channelBuffers32: side_channels.as_mut_ptr(),
            },
        };
        // Hand over the buses the plugin declared, in its order. An
        // instrument has no main input, so its sidechain comes first.
        let has_main_input = self
            .bus_info(AUDIO, INPUT, 0)
            .is_some_and(|bus| bus.busType == vst::BusTypes_::kMain as vst::BusType);
        let mut inputs: Vec<vst::AudioBusBuffers> = match (has_main_input, side_channels.is_empty())
        {
            (true, true) => vec![main_bus],
            (true, false) => vec![main_bus, side_bus],
            (false, false) => vec![side_bus],
            (false, true) => vec![],
        };
        let mut outputs = [main_bus];

        let changes = ComWrapper::new(Changes {
            queues: changes
                .into_iter()
                .map(|(id, points)| ComWrapper::new(Queue { id, points }))
                .collect(),
        });
        let events = ComWrapper::new(Events { events });
        let mut context: vst::ProcessContext = unsafe { std::mem::zeroed() };
        if let Some(tempo) = tempo {
            use vst::ProcessContext_::StatesAndFlags_ as flags;
            context.state = (flags::kPlaying | flags::kTempoValid) as u32;
            context.tempo = tempo;
        }

        let mut data = vst::ProcessData {
            processMode: vst::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: vst::SymbolicSampleSizes_::kSample32 as i32,
            numSamples: frames as i32,
            numInputs: inputs.len() as i32,
            numOutputs: 1,
            inputs: inputs.as_mut_ptr(),
            outputs: outputs.as_mut_ptr(),
            inputParameterChanges: changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr(),
            outputParameterChanges: ptr::null_mut(),
            inputEvents: events.as_com_ref::<IEventList>().unwrap().as_ptr(),
            outputEvents: ptr::null_mut(),
            processContext: if tempo.is_some() {
                &raw mut context
            } else {
                ptr::null_mut()
            },
        };

        HEAP_CALLS.with(|calls| calls.set(0));
        IN_PROCESS.with(|watching| watching.set(true));
        let status = unsafe { self.processor.process(&mut data) };
        IN_PROCESS.with(|watching| watching.set(false));
        Processed {
            status,
            output: main.to_vec(),
            heap_calls: HEAP_CALLS.with(Cell::get),
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
            self.component.terminate();
        }
    }
}

const AUDIO: u32 = vst::MediaTypes_::kAudio as u32;
const EVENT: u32 = vst::MediaTypes_::kEvent as u32;
const INPUT: u32 = vst::BusDirections_::kInput as u32;
const OUTPUT: u32 = vst::BusDirections_::kOutput as u32;

fn narrow(field: &[c_char]) -> String {
    unsafe { CStr::from_ptr(field.as_ptr()).to_str().unwrap().to_owned() }
}

fn wide(field: &[u16]) -> String {
    let len = field.iter().position(|&unit| unit == 0).unwrap();
    String::from_utf16(&field[..len]).unwrap()
}

/// The plugins under test share globals, so the tests that use `Level` must
/// not run at the same time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn level_host() -> (std::sync::MutexGuard<'static, ()>, Host) {
    let guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // Through the exported entry point, as a host would.
    let host = Host::new::<Level>(GetPluginFactory());
    host.activate(48_000.0, 64);
    (guard, host)
}

fn stereo(value: f32, frames: usize) -> Vec<Vec<f32>> {
    vec![vec![value; frames]; 2]
}

// --- The tests -----------------------------------------------------------

#[test]
fn the_heap_watcher_itself_works() {
    HEAP_CALLS.with(|calls| calls.set(0));
    IN_PROCESS.with(|watching| watching.set(true));
    drop(std::hint::black_box(vec![0u8; 64]));
    IN_PROCESS.with(|watching| watching.set(false));
    assert!(
        HEAP_CALLS.with(Cell::get) >= 2,
        "an allocation went unnoticed"
    );
}

#[test]
fn the_factory_describes_and_creates_the_plugin() {
    let (_guard, host) = level_host();
    unsafe {
        let mut factory_info: sb::PFactoryInfo = std::mem::zeroed();
        assert_eq!(host.factory.getFactoryInfo(&mut factory_info), kResultOk);
        assert_eq!(narrow(&factory_info.vendor), "Manguebyte");
        assert_ne!(
            factory_info.flags & sb::PFactoryInfo_::FactoryFlags_::kUnicode as i32,
            0
        );
        assert_eq!(host.factory.countClasses(), 1);

        let mut basic: sb::PClassInfo = std::mem::zeroed();
        assert_eq!(host.factory.getClassInfo(0, &mut basic), kResultOk);
        assert_eq!(basic.cid, class_id("org.manguebyte.test.level"));
        assert_eq!(narrow(&basic.category), "Audio Module Class");
        assert_eq!(narrow(&basic.name), "Level");
        assert_eq!(host.factory.getClassInfo(1, &mut basic), kInvalidArgument);

        let second: ComPtr<IPluginFactory2> = host.factory.cast().unwrap();
        let mut detailed: sb::PClassInfo2 = std::mem::zeroed();
        assert_eq!(second.getClassInfo2(0, &mut detailed), kResultOk);
        assert_eq!(narrow(&detailed.subCategories), "Fx");
        assert_eq!(narrow(&detailed.vendor), "Manguebyte");
        assert_eq!(narrow(&detailed.version), "1.2.3");
        assert!(narrow(&detailed.sdkVersion).starts_with("VST 3."));

        let third: ComPtr<IPluginFactory3> = host.factory.cast().unwrap();
        let mut unicode: sb::PClassInfoW = std::mem::zeroed();
        assert_eq!(third.getClassInfoUnicode(0, &mut unicode), kResultOk);
        assert_eq!(wide(&unicode.name), "Level");
        assert_eq!(wide(&unicode.version), "1.2.3");
        assert_eq!(third.setHostContext(ptr::null_mut()), kResultOk);

        // Another plugin's class id is refused and nothing is handed out.
        let other = class_id("org.example.other");
        let mut object = ptr::dangling_mut::<c_void>();
        assert_eq!(
            host.factory
                .createInstance(other.as_ptr(), iid::<IComponent>(), &mut object),
            kInvalidArgument
        );
        assert!(object.is_null());
    }
}

#[test]
fn buses_and_sample_formats_follow_the_layout() {
    let (_guard, host) = level_host();
    assert_eq!(host.bus_count(AUDIO, INPUT), 1);
    assert_eq!(host.bus_count(AUDIO, OUTPUT), 1);
    assert_eq!(host.bus_count(EVENT, INPUT), 0, "an effect takes no notes");
    assert_eq!(host.bus_count(EVENT, OUTPUT), 0);

    for (direction, name) in [(INPUT, "Input"), (OUTPUT, "Output")] {
        let bus = host.bus_info(AUDIO, direction, 0).unwrap();
        assert_eq!(wide(&bus.name), name);
        assert_eq!(bus.channelCount, 2);
        assert_eq!(bus.busType, vst::BusTypes_::kMain as vst::BusType);
        assert_ne!(
            bus.flags & vst::BusInfo_::BusFlags_::kDefaultActive as u32,
            0
        );
        assert!(host.bus_info(AUDIO, direction, 1).is_none());
    }
    assert!(host.bus_info(EVENT, INPUT, 0).is_none());

    unsafe {
        let mut arrangement = 0u64;
        assert_eq!(
            host.processor
                .getBusArrangement(OUTPUT as _, 0, &mut arrangement),
            kResultOk
        );
        assert_eq!(arrangement, vst::SpeakerArr::kStereo);

        // Level offers one layout: its own is accepted, others are not.
        let (mut stereo, mut mono) = (vst::SpeakerArr::kStereo, vst::SpeakerArr::kMono);
        let mut output = vst::SpeakerArr::kStereo;
        assert_eq!(
            host.processor
                .setBusArrangements(&mut stereo, 1, &mut output, 1),
            kResultOk
        );
        assert_eq!(
            host.processor
                .setBusArrangements(&mut mono, 1, &mut output, 1),
            kResultFalse
        );
        assert_eq!(
            host.processor
                .setBusArrangements(&mut stereo, 0, &mut output, 1),
            kResultFalse
        );

        let single = vst::SymbolicSampleSizes_::kSample32 as i32;
        let double = vst::SymbolicSampleSizes_::kSample64 as i32;
        assert_eq!(host.processor.canProcessSampleSize(single), kResultOk);
        // 64-bit buffers go through the same 32-bit path.
        assert_eq!(host.processor.canProcessSampleSize(double), kResultOk);
        assert_eq!(host.processor.canProcessSampleSize(7), kResultFalse);
        assert_eq!(host.processor.getLatencySamples(), 7);

        // One object is both processor and controller.
        let mut controller_class = [0 as c_char; 16];
        assert_eq!(
            host.component.getControllerClassId(&mut controller_class),
            kNotImplemented
        );
        let requirements: ComPtr<IProcessContextRequirements> = host.component.cast().unwrap();
        assert_ne!(requirements.getProcessContextRequirements(), 0);
    }
}

#[test]
fn parameters_are_listed_with_values_and_text() {
    use vst::ParameterInfo_::ParameterFlags_ as flags;
    let (_guard, host) = level_host();
    let level_id = param_id(LEVEL);
    unsafe { assert_eq!(host.controller.getParameterCount(), 3) };

    let level = host.param_info(0).unwrap();
    assert_eq!(level.id, level_id);
    assert!(level.id < 0x8000_0000, "the top bit is reserved for hosts");
    assert_eq!(wide(&level.title), "Level");
    assert_eq!(wide(&level.units), "x");
    assert_eq!(level.stepCount, 0);
    assert_eq!(level.defaultNormalizedValue, 0.25);
    assert_eq!(level.flags, flags::kCanAutomate as i32);

    let shape = host.param_info(1).unwrap();
    assert_eq!(shape.stepCount, 2);
    assert_eq!(shape.defaultNormalizedValue, 0.5);
    let explode = host.param_info(2).unwrap();
    assert_ne!(explode.flags & flags::kIsHidden as i32, 0);
    assert!(host.param_info(3).is_none());
    assert!(host.param_info(-1).is_none());

    assert_eq!(host.param_text(level_id, 0.5).unwrap(), "2.00");
    assert_eq!(host.param_from_text(level_id, "3 x"), Some(0.75));
    assert_eq!(host.param_from_text(level_id, "loud"), None);
    assert_eq!(host.param_text(param_id(SHAPE), 1.0).unwrap(), "Square");
    assert_eq!(host.param_from_text(param_id(SHAPE), "sine"), Some(0.0));
    assert!(host.param_text(0x1234_5678, 0.0).is_none());

    unsafe {
        let controller = &host.controller;
        assert_eq!(controller.normalizedParamToPlain(level_id, 0.5), 2.0);
        assert_eq!(controller.plainParamToNormalized(level_id, 3.0), 0.75);
        assert_eq!(controller.getParamNormalized(level_id), 0.25);
        assert_eq!(controller.setParamNormalized(level_id, 1.0), kResultOk);
        assert_eq!(controller.getParamNormalized(level_id), 1.0);
        assert_eq!(controller.setParamNormalized(level_id, 7.0), kResultOk);
        assert_eq!(controller.getParamNormalized(level_id), 1.0, "not clamped");
        assert_eq!(
            controller.setParamNormalized(level_id, f64::NAN),
            kInvalidArgument
        );
        assert_eq!(
            controller.setParamNormalized(0x1234_5678, 1.0),
            kInvalidArgument
        );
    }
}

#[test]
fn automation_lands_on_its_frame_when_processing_in_place() {
    let (_guard, host) = level_host();
    let mut audio = stereo(1.0, 64);
    let changes = vec![(param_id(LEVEL), vec![(10, 0.5), (40, 1.0)])];
    let processed = host.process(&mut audio, 64, None, changes, vec![], Some(97.0));

    assert_eq!(processed.status, kResultOk);
    assert_eq!(processed.heap_calls, 0, "process used the heap");
    for channel in &processed.output {
        assert_eq!(&channel[..10], &[1.0; 10]);
        assert_eq!(&channel[10..40], &[2.0; 30]);
        assert_eq!(&channel[40..], &[4.0; 24]);
    }
    assert_eq!(level_params().level.value(), 4.0);
    let tempo = TEMPO_SEEN.lock().unwrap().clone().unwrap();
    assert_eq!(tempo.load(Ordering::Relaxed), 97.0);
}

#[test]
fn what_the_controller_is_told_does_not_disturb_running_automation() {
    let (_guard, host) = level_host();
    let level_id = param_id(LEVEL);
    let changes = vec![(level_id, vec![(0, 0.5)])];
    let processed = host.process(&mut stereo(1.0, 16), 16, None, changes, vec![], None);
    assert_eq!(processed.output[0], vec![2.0; 16]);

    // While it plays automation a host also tells the controller, from the
    // main thread, so that an editor can follow. That value is early or
    // late compared with the points the audio thread gets.
    unsafe {
        assert_eq!(host.controller.setParamNormalized(level_id, 1.0), kResultOk);
        assert_eq!(host.controller.getParamNormalized(level_id), 1.0);
    }
    assert_eq!(level_params().level.value(), 2.0);

    // The next point is on frame 20. Until then the plugin stays where the
    // last point left it, rather than jumping to the controller's value.
    let changes = vec![(level_id, vec![(20, 0.75)])];
    let processed = host.process(&mut stereo(1.0, 32), 32, None, changes, vec![], None);
    assert_eq!(&processed.output[0][..20], &[2.0; 20]);
    assert_eq!(&processed.output[0][20..], &[3.0; 12]);
    assert_eq!(processed.heap_calls, 0);

    let processed = host.process(&mut stereo(1.0, 16), 16, None, vec![], vec![], None);
    assert_eq!(processed.output[0], vec![3.0; 16]);
}

#[test]
fn a_value_set_while_no_blocks_run_is_not_lost() {
    let _guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let host = Host::new::<Level>(plugin_factory::<Level>());
    let level_id = param_id(LEVEL);

    // Before the plugin is ever activated there are no blocks to carry the
    // change, so the controller's value is the only one there will be.
    unsafe {
        assert_eq!(
            host.controller.setParamNormalized(level_id, 0.75),
            kResultOk
        );
    }
    assert_eq!(level_params().level.value(), 3.0);
    host.activate(48_000.0, 64);
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.output[0], vec![3.0; 8]);

    // The same when the host stops processing for a while.
    unsafe {
        assert_eq!(host.processor.setProcessing(0), kResultOk);
        assert_eq!(host.controller.setParamNormalized(level_id, 0.5), kResultOk);
        assert_eq!(host.processor.setProcessing(1), kResultOk);
    }
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.output[0], vec![2.0; 8]);
    assert_eq!(processed.heap_calls, 0);
}

#[test]
fn state_survives_a_round_trip_through_host_streams() {
    let (_guard, host) = level_host();
    level_params().level.set(2.5);
    level_params().shape.set_index(0);
    let saved = host.save().unwrap();
    let as_text = String::from_utf8(saved.clone()).unwrap();
    assert!(as_text.starts_with("manguebyte-plugin 1 org.manguebyte.test.level\n"));

    // A change the plugin makes to its own parameter, as its editor would,
    // reaches the audio thread as an event on the next block.
    level_params().level.set(0.5);
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.output[0], vec![0.5; 8]);

    assert_eq!(host.load(&saved), kResultOk);
    unsafe {
        assert_eq!(host.controller.getParamNormalized(param_id(LEVEL)), 0.625);
        // The controller is the same object, so this is a formality.
        assert_eq!(
            host.controller.setComponentState(ptr::null_mut()),
            kResultOk
        );
        assert_eq!(host.controller.getParamNormalized(param_id(LEVEL)), 0.625);
    }
    // The audio thread hears about the restored value as an event.
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.output[0], vec![2.5; 8]);
    assert_eq!(processed.heap_calls, 0);

    assert_eq!(host.load(b"not a state"), kResultFalse);
    assert_eq!(host.load(&[0xFF, 0xFE, 0x00]), kResultFalse);
    let foreign = as_text.replace("test.level", "test.other");
    assert_eq!(host.load(foreign.as_bytes()), kResultFalse);
    assert_eq!(
        level_params().level.value(),
        2.5,
        "a refused load changed a value"
    );
}

/// The automation points of one block, handed to the plugin by a host
/// whose main thread stores a value into the level parameter at the moment
/// the plugin asks how many queues there are: after it has read the
/// parameters to announce their changes to itself, and before it stores
/// the host's values.
struct Meddling {
    params: Arc<LevelParams>,
    level: Cell<Option<f32>>,
}

impl Class for Meddling {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Meddling {
    unsafe fn getParameterCount(&self) -> i32 {
        if let Some(level) = self.level.take() {
            self.params.level.set(level);
        }
        0
    }
    unsafe fn getParameterData(&self, _index: i32) -> *mut IParamValueQueue {
        ptr::null_mut()
    }
    unsafe fn addParameterData(&self, _id: *const u32, _index: *mut i32) -> *mut IParamValueQueue {
        ptr::null_mut()
    }
}

/// A value the main thread stores while a block is being prepared is not
/// undone by the value announced a moment earlier, and the plugin hears
/// about it on the next block.
#[test]
fn a_value_stored_while_a_block_is_prepared_is_kept_and_announced() {
    let (_guard, host) = level_host();
    // Changed outside the host, as a state load or an editor would.
    level_params().level.set(0.5);
    let meddling = ComWrapper::new(Meddling {
        params: level_params(),
        level: Cell::new(Some(3.0)),
    });
    let mut main = stereo(1.0, 8);
    let mut channels: Vec<*mut f32> = main.iter_mut().map(|c| c.as_mut_ptr()).collect();
    let bus = vst::AudioBusBuffers {
        numChannels: 2,
        silenceFlags: 0,
        __field0: vst::AudioBusBuffers__type0 {
            channelBuffers32: channels.as_mut_ptr(),
        },
    };
    let (mut inputs, mut outputs) = ([bus], [bus]);
    let mut data = vst::ProcessData {
        processMode: vst::ProcessModes_::kRealtime as i32,
        symbolicSampleSize: vst::SymbolicSampleSizes_::kSample32 as i32,
        numSamples: 8,
        numInputs: 1,
        numOutputs: 1,
        inputs: inputs.as_mut_ptr(),
        outputs: outputs.as_mut_ptr(),
        inputParameterChanges: meddling.as_com_ref::<IParameterChanges>().unwrap().as_ptr(),
        outputParameterChanges: ptr::null_mut(),
        inputEvents: ptr::null_mut(),
        outputEvents: ptr::null_mut(),
        processContext: ptr::null_mut(),
    };
    assert_eq!(unsafe { host.processor.process(&mut data) }, kResultOk);
    // The block was announced the value read before the store.
    assert_eq!(main[0], vec![0.5; 8]);
    assert_eq!(
        level_params().level.value(),
        3.0,
        "the newer value was undone by the announced one"
    );
    // And the next block is told about the newer one.
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.output[0], vec![3.0; 8]);
    assert_eq!(processed.heap_calls, 0);
}

#[test]
fn a_plugin_without_an_editor_offers_no_view() {
    let (_guard, host) = level_host();
    unsafe {
        let editor = vst::ViewType::kEditor;
        assert!(host.controller.createView(editor).is_null());
        assert!(host.controller.createView(ptr::null()).is_null());
        // The handler is accepted, and taken away again, all the same.
        assert_eq!(
            host.controller.setComponentHandler(ptr::null_mut()),
            kResultOk
        );
    }
}

#[test]
fn a_block_with_no_audio_still_delivers_parameter_changes() {
    let (_guard, host) = level_host();
    let changes = vec![(param_id(LEVEL), vec![(0, 0.75)])];
    let processed = host.process(&mut stereo(1.0, 8), 0, None, changes, vec![], None);
    assert_eq!(processed.status, kResultOk);
    assert_eq!(processed.output[0], vec![1.0; 8], "audio was touched");
    assert_eq!(level_params().level.value(), 3.0);

    // The plugin, which only follows events, has the value by the next
    // block with audio.
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.output[0], vec![3.0; 8]);
    assert_eq!(processed.heap_calls, 0);
}

#[test]
fn controller_moves_in_a_block_with_no_audio_are_kept_for_the_next() {
    let host = Host::new::<Keys>(plugin_factory::<Keys>());
    host.activate(48_000.0, 32);
    let mapping: ComPtr<IMidiMapping> = host.component.cast().unwrap();
    let mut bend = 0u32;
    let pitch_bend = vst::ControllerNumbers_::kPitchBend as i16;
    unsafe {
        assert_eq!(
            mapping.getMidiControllerAssignment(0, 0, pitch_bend, &mut bend),
            kResultOk
        );
    }

    // The wheel is moved while the host is stopped. A wheel position is
    // not a parameter of the plugin, so there is no stored value for the
    // next block to find: the move itself has to survive.
    let changes = vec![(bend, vec![(0, 0.0)])];
    let processed = host.process(&mut [vec![0.0f32; 4]], 0, None, changes, vec![], None);
    assert_eq!(processed.status, kResultOk);
    assert_eq!(processed.heap_calls, 0);

    let events = vec![note_on(0, 60, -1, 0.0)];
    let processed = host.process(&mut [vec![0.0f32; 4]], 4, None, vec![], events, None);
    // All the way down is two semitones with the default range.
    assert_eq!(processed.output[0], vec![58.0; 4]);
    assert_eq!(processed.heap_calls, 0);
}

#[test]
fn a_block_larger_than_promised_and_a_panic_are_errors_not_crashes() {
    let (_guard, host) = level_host();
    let processed = host.process(&mut stereo(1.0, 65), 65, None, vec![], vec![], None);
    assert_eq!(processed.status, kResultFalse);
    assert_eq!(processed.heap_calls, 0, "refusing a block used the heap");

    level_params().explode.set(true);
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.status, kResultFalse);

    // The plugin is still usable afterwards.
    level_params().explode.set(false);
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.status, kResultOk);
}

/// A point the host sent in a block the plugin panicked in was stored, but
/// never reached the plugin, which follows events alone. The next block
/// gives it every value again, so it does not keep playing the old one.
#[test]
fn after_a_panic_the_next_block_gives_the_plugin_every_value() {
    let (_guard, host) = level_host();
    let level_id = param_id(LEVEL);
    level_params().explode.set(true);
    let changes = vec![(level_id, vec![(0, 0.5)])];
    let processed = host.process(&mut stereo(1.0, 8), 8, None, changes, vec![], None);
    assert_eq!(processed.status, kResultFalse);
    assert_eq!(level_params().level.value(), 2.0, "the value was stored");

    level_params().explode.set(false);
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.status, kResultOk);
    assert_eq!(
        processed.output[0],
        vec![2.0; 8],
        "the plugin kept the value from before the panic"
    );
    // Only once: the block after that announces nothing new.
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.output[0], vec![2.0; 8]);
    assert_eq!(processed.heap_calls, 0);
}

#[test]
fn numbers_too_small_to_hear_are_zero_while_the_plugin_runs_and_only_then() {
    let (_guard, host) = level_host();
    assert!(half_of_the_smallest() > 0.0, "the test thread flushes them");
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.status, kResultOk);

    let inside = f32::from_bits(HALF_INSIDE_PROCESS.load(Ordering::Relaxed));
    if NoDenormals::SUPPORTED {
        assert_eq!(inside, 0.0, "the plugin ran with subnormal numbers on");
    }
    // The host gets its own setting back.
    assert!(
        half_of_the_smallest() > 0.0,
        "the setting leaked into the host"
    );

    // Also when the plugin fails half-way.
    level_params().explode.set(true);
    host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert!(
        half_of_the_smallest() > 0.0,
        "the setting leaked after a panic"
    );
}

#[test]
fn processing_before_activation_is_refused() {
    let _guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let host = Host::new::<Level>(plugin_factory::<Level>());
    let processed = host.process(&mut stereo(1.0, 8), 8, None, vec![], vec![], None);
    assert_eq!(processed.status, kResultFalse);
    assert_eq!(processed.output[0], vec![1.0; 8]);
}

#[test]
fn an_instrument_takes_notes_expressions_controllers_and_a_sidechain() {
    let host = Host::new::<Keys>(plugin_factory::<Keys>());
    host.activate(48_000.0, 32);

    // No main input: the only input bus is the stereo sidechain.
    assert_eq!(host.bus_count(AUDIO, INPUT), 1);
    let sidechain = host.bus_info(AUDIO, INPUT, 0).unwrap();
    assert_eq!(wide(&sidechain.name), "Sidechain");
    assert_eq!(sidechain.channelCount, 2);
    assert_eq!(sidechain.busType, vst::BusTypes_::kAux as vst::BusType);
    assert_eq!(sidechain.flags, 0, "a sidechain starts switched off");
    let output = host.bus_info(AUDIO, OUTPUT, 0).unwrap();
    assert_eq!(output.channelCount, 1);
    assert_eq!(host.bus_count(EVENT, INPUT), 1);
    assert_eq!(wide(&host.bus_info(EVENT, INPUT, 0).unwrap().name), "Notes");
    unsafe {
        let mut arrangement = 0u64;
        host.processor
            .getBusArrangement(OUTPUT as _, 0, &mut arrangement);
        assert_eq!(arrangement, vst::SpeakerArr::kMono);
    }

    // MIDI controllers arrive as parameters. `Keys` declares nothing, so it
    // has the musical default: one hidden parameter for each of its 13
    // controllers, shared by every channel, after the plugin's own.
    assert_eq!(Keys::MIDI_CONTROLLERS, MidiControllers::MUSICAL);
    let mapping: ComPtr<IMidiMapping> = host.component.cast().unwrap();
    let controller_id = |channel: i16, controller: u32| unsafe {
        let mut id = 0u32;
        (mapping.getMidiControllerAssignment(0, channel, controller as i16, &mut id) == kResultOk)
            .then_some(id)
    };
    let bend = controller_id(0, vst::ControllerNumbers_::kPitchBend as u32).unwrap();
    let sustain = controller_id(0, 64).unwrap();
    assert_ne!(bend, sustain);
    assert_eq!(controller_id(1, 129), Some(bend));
    assert_eq!(controller_id(15, 64), Some(sustain));
    assert!(controller_id(16, 64).is_none());
    assert!(controller_id(0, 130).is_none());
    // Not in the musical set.
    assert!(controller_id(0, 74).is_none());
    unsafe {
        assert_eq!(host.controller.getParameterCount(), 1 + 13);
        // Pitch bend rests in the middle.
        assert_eq!(host.controller.getParamNormalized(bend), 0.5);
    }
    // In the order of the controllers: 1, 2, 7, 10, 11, then 64.
    let hidden = host.param_info(1 + 5).unwrap();
    assert_eq!(hidden.id, sustain);
    assert_eq!(wide(&hidden.title), "MIDI CC 64");
    assert_ne!(
        hidden.flags & vst::ParameterInfo_::ParameterFlags_::kIsHidden as i32,
        0
    );
    assert_eq!(
        wide(&host.param_info(1 + 12).unwrap().title),
        "MIDI Pitch Bend"
    );
    assert_eq!(
        wide(&host.param_info(1 + 11).unwrap().title),
        "MIDI Pressure"
    );

    // Halving the volume of the note with id 5: a quarter of the range is
    // unity, so an eighth is half.
    let mut volume = event(
        12,
        vst::Event_::EventTypes_::kNoteExpressionValueEvent as u32,
    );
    volume.__field0.noteExpressionValue = vst::NoteExpressionValueEvent {
        typeId: vst::NoteExpressionTypeIDs_::kVolumeTypeID as u32,
        noteId: 5,
        value: 0.125,
    };
    let events = vec![
        note_on(4, 60, 5, 0.0),
        note_on(8, 72, -1, 0.0),
        volume,
        note_off(20, 60, 5),
        note_off(24, 72, -1),
    ];
    let mut audio = vec![vec![9.0f32; 32]];
    let processed = host.process(&mut audio, 32, None, vec![], events, None);
    assert_eq!(processed.status, kResultOk);
    assert_eq!(processed.heap_calls, 0, "playing notes used the heap");
    let out = &processed.output[0];
    assert_eq!(&out[..4], &[0.0; 4]);
    assert_eq!(&out[4..8], &[60.0; 4]);
    assert_eq!(&out[8..12], &[132.0; 4]);
    assert_eq!(&out[12..20], &[102.0; 8]);
    assert_eq!(&out[20..24], &[72.0; 4]);
    assert_eq!(&out[24..], &[0.0; 8]);

    // A note that starts 50 cents sharp, polyphonic pressure, the sustain
    // pedal and the pitch wheel, with a sidechain underneath.
    let mut pressure = event(4, vst::Event_::EventTypes_::kPolyPressureEvent as u32);
    pressure.__field0.polyPressure = vst::PolyPressureEvent {
        channel: 0,
        pitch: 50,
        pressure: 0.25,
        noteId: -1,
    };
    let events = vec![note_on(0, 50, -1, 50.0), pressure, note_off(12, 50, -1)];
    let changes = vec![
        // Pedal down before the note is released, up later.
        (sustain, vec![(8, 1.0), (20, 0.0)]),
        // The wheel all the way up: two semitones with the default range.
        (bend, vec![(16, 1.0)]),
    ];
    let mut side = stereo(0.25, 24);
    let processed = host.process(
        &mut [vec![0.0f32; 24]],
        24,
        Some(&mut side),
        changes,
        events,
        None,
    );
    assert_eq!(processed.heap_calls, 0);
    let out = &processed.output[0];
    // 50 + 0.5 semitones of tuning, plus the sidechain's 0.25.
    assert_eq!(&out[..4], &[50.75; 4]);
    // Pressure of 0.25 shows up as 25.
    assert_eq!(&out[4..16], &[75.25; 12], "the pedal did not hold the note");
    // The wheel adds two semitones, less the one step in 8192 that MIDI
    // pitch bend cannot reach.
    for &sample in &out[16..20] {
        assert!((sample - 77.25).abs() < 0.001, "{sample}");
    }
    // Pedal up: the note, released at frame 12, finally stops.
    assert_eq!(&out[20..], &[0.25; 4]);
}

#[test]
fn hidden_controller_parameters_are_filed_in_their_own_unit() {
    let host = Host::new::<Keys>(plugin_factory::<Keys>());
    let units: ComPtr<IUnitInfo> = host.component.cast().unwrap();
    unsafe {
        assert_eq!(units.getUnitCount(), 2);
        let mut root: vst::UnitInfo = std::mem::zeroed();
        assert_eq!(units.getUnitInfo(0, &mut root), kResultOk);
        assert_eq!(root.id, vst::kRootUnitId);
        assert_eq!(root.parentUnitId, vst::kNoParentUnitId);
        assert_eq!(root.programListId, vst::kNoProgramListId);

        let mut midi: vst::UnitInfo = std::mem::zeroed();
        assert_eq!(units.getUnitInfo(1, &mut midi), kResultOk);
        assert_eq!(wide(&midi.name), "MIDI Controllers");
        assert_eq!(midi.parentUnitId, vst::kRootUnitId);
        assert_eq!(units.getUnitInfo(2, &mut midi), kInvalidArgument);

        // The plugin's own parameter is in the root; every controller
        // parameter is in the other unit.
        assert_eq!(host.param_info(0).unwrap().unitId, root.id);
        let count = host.controller.getParameterCount();
        for index in 1..count {
            assert_eq!(host.param_info(index).unwrap().unitId, midi.id);
        }

        assert_eq!(units.getProgramListCount(), 0);
        assert_eq!(units.getSelectedUnit(), vst::kRootUnitId);
        assert_eq!(units.selectUnit(midi.id), kResultOk);
        assert_eq!(units.getSelectedUnit(), midi.id);
        assert_eq!(units.selectUnit(9), kInvalidArgument);
        assert_eq!(units.getSelectedUnit(), midi.id);
    }
    drop(host);

    // An effect has no controller parameters and so only the root.
    let _guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let effect = Host::new::<Level>(plugin_factory::<Level>());
    let units: ComPtr<IUnitInfo> = effect.component.cast().unwrap();
    unsafe { assert_eq!(units.getUnitCount(), 1) };
}

#[test]
fn note_expressions_are_announced_for_instruments_only() {
    let host = Host::new::<Keys>(plugin_factory::<Keys>());
    let expressions: ComPtr<INoteExpressionController> = host.component.cast().unwrap();
    unsafe {
        assert_eq!(expressions.getNoteExpressionCount(0, 0), 4);
        assert_eq!(expressions.getNoteExpressionCount(1, 0), 0);
        let mut info: vst::NoteExpressionTypeInfo = std::mem::zeroed();
        assert_eq!(
            expressions.getNoteExpressionInfo(0, 0, 2, &mut info),
            kResultOk
        );
        assert_eq!(
            info.typeId,
            vst::NoteExpressionTypeIDs_::kTuningTypeID as u32
        );
        assert_eq!(wide(&info.title), "Tuning");
        assert_eq!(info.valueDesc.defaultValue, 0.5);
        assert_eq!(
            expressions.getNoteExpressionInfo(0, 0, 4, &mut info),
            kInvalidArgument
        );

        // The middle of the tuning range is no detuning at all.
        let mut text = [0u16; 128];
        let tuning = vst::NoteExpressionTypeIDs_::kTuningTypeID as u32;
        assert_eq!(
            expressions.getNoteExpressionStringByValue(0, 0, tuning, 0.5, &mut text),
            kResultOk
        );
        assert_eq!(wide(&text), "0.00");

        // And text back to a position: an octave up is a twentieth of the
        // way from the middle to the top.
        let position = |text: &str| {
            let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
            let mut value = f64::NAN;
            let result = expressions.getNoteExpressionValueByString(
                0,
                0,
                tuning,
                units.as_ptr(),
                &mut value,
            );
            (result == kResultOk).then_some(value)
        };
        assert_eq!(position("12 st"), Some(0.55));
        assert_eq!(position("-120"), Some(0.0));
        assert_eq!(position("sharp"), None);
    }
    drop(host);

    let _guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let effect = Host::new::<Level>(plugin_factory::<Level>());
    let expressions: ComPtr<INoteExpressionController> = effect.component.cast().unwrap();
    unsafe { assert_eq!(expressions.getNoteExpressionCount(0, 0), 0) };
}

// --- Declared MIDI controllers ---------------------------------------------

/// The first id of the range reserved for controller parameters, as the
/// crate documents it.
const MIDI_BASE: u32 = 0x7F00_0000;

/// Miri interprets every call, so under it the exhaustive sweeps below
/// take every `SPARSE`th case and the random runs are shorter.
const SPARSE: usize = if cfg!(miri) { 37 } else { 1 };
const ROUNDS: usize = if cfg!(miri) { 12 } else { 300 };

/// What a `Probe` declares, and where it writes down what it hears.
trait Declaration: 'static {
    const ID: &'static str;
    const CONTROLLERS: MidiControllers;
    const KIND: PluginKind = PluginKind::Instrument;
    fn heard() -> &'static Mutex<Vec<(u32, MidiMessage)>>;
}

struct ProbeParams {
    gain: FloatParam,
}
param_set!(ProbeParams { gain });

/// Writes down every MIDI message it is given, with its frame.
struct Probe<D> {
    params: Arc<ProbeParams>,
    declaration: PhantomData<fn() -> D>,
}

impl<D: Declaration> Plugin for Probe<D> {
    const INFO: PluginInfo = PluginInfo {
        id: D::ID,
        name: "Probe",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Writes down the MIDI it hears",
        kind: D::KIND,
    };
    const LAYOUT: AudioLayout = AudioLayout::MONO_EFFECT;
    const MIDI_CONTROLLERS: MidiControllers = D::CONTROLLERS;
    type Params = ProbeParams;

    fn new() -> Self {
        Self {
            params: Arc::new(ProbeParams {
                gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5),
            }),
            declaration: PhantomData,
        }
    }
    fn params(&self) -> Arc<ProbeParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        // A lock that is never contended, and room made before the block:
        // neither touches the heap.
        let mut heard = D::heard().lock().unwrap();
        for event in context.events {
            if let EventKind::Midi(message) = event.kind {
                if heard.len() < heard.capacity() {
                    heard.push((event.offset, message));
                }
            }
        }
    }
}

macro_rules! declaration {
    ($name:ident, $id:literal, $controllers:expr $(, $kind:expr)?) => {
        struct $name;
        impl Declaration for $name {
            const ID: &'static str = $id;
            const CONTROLLERS: MidiControllers = $controllers;
            $(const KIND: PluginKind = $kind;)?
            fn heard() -> &'static Mutex<Vec<(u32, MidiMessage)>> {
                static HEARD: Mutex<Vec<(u32, MidiMessage)>> = Mutex::new(Vec::new());
                &HEARD
            }
        }
    };
}

declaration!(
    Musical,
    "org.manguebyte.test.musical",
    MidiControllers::MUSICAL
);
declaration!(
    Nothing,
    "org.manguebyte.test.nothing",
    MidiControllers::NONE
);
declaration!(
    Listed,
    "org.manguebyte.test.listed",
    MidiControllers::list(&[
        MidiController::control(74).unwrap(),
        MidiController::PITCH_BEND,
        MidiController::control(0).unwrap(),
    ])
);
declaration!(
    Channels,
    "org.manguebyte.test.channels",
    MidiControllers::MUSICAL.per_channel()
);
declaration!(Shared, "org.manguebyte.test.shared", MidiControllers::ALL);
declaration!(
    Everything,
    "org.manguebyte.test.everything",
    MidiControllers::ALL.per_channel()
);
declaration!(
    EffectWithAll,
    "org.manguebyte.test.effect",
    MidiControllers::ALL.per_channel(),
    PluginKind::Effect
);

/// The probes share their notebooks, so their tests take turns.
static PROBES: Mutex<()> = Mutex::new(());

/// The controllers a probe of declaration `D` should expose: none for an
/// effect, which has no event bus.
fn declared<D: Declaration>() -> MidiControllers {
    if D::KIND == PluginKind::Effect {
        MidiControllers::NONE
    } else {
        D::CONTROLLERS
    }
}

/// The hidden parameters a declaration should give, in the host's order,
/// with the channel each one is for (`None` when shared by all).
fn expected_hidden(set: MidiControllers) -> Vec<(u32, Option<u32>, MidiController)> {
    let channels = if set.is_per_channel() { 16 } else { 1 };
    let mut hidden = Vec::new();
    for channel in 0..channels {
        for controller in set.iter() {
            let id = MIDI_BASE + channel * 130 + controller.index() as u32;
            hidden.push((id, set.is_per_channel().then_some(channel), controller));
        }
    }
    hidden
}

/// The parameter IMidiMapping should give for `controller` on `channel` of
/// bus `bus`, if any.
fn expected_mapping(set: MidiControllers, bus: i32, channel: i16, controller: i16) -> Option<u32> {
    let channel = u32::try_from(channel)
        .ok()
        .filter(|&channel| channel < 16)?;
    let controller = MidiController::from_index(usize::try_from(controller).ok()?)?;
    if bus != 0 || !set.contains(controller) {
        return None;
    }
    let own = if set.is_per_channel() { channel } else { 0 };
    Some(MIDI_BASE + own * 130 + controller.index() as u32)
}

/// `controller` on `channel` at `value`, which is 0 to 127, or -8,192 to
/// 8,191 for pitch bend.
fn message(channel: u8, controller: MidiController, value: i32) -> MidiMessage {
    match controller.control_number() {
        Some(number) => MidiMessage::ControlChange {
            channel,
            controller: number,
            value: u8::try_from(value).unwrap(),
        },
        None if controller == MidiController::PITCH_BEND => MidiMessage::PitchBend {
            channel,
            value: i16::try_from(value).unwrap(),
        },
        None => MidiMessage::ChannelPressure {
            channel,
            pressure: u8::try_from(value).unwrap(),
        },
    }
}

/// What the plugin should hear when the hidden parameter `id` moves to a
/// position whose MIDI value is `value`: nothing for an id it does not
/// have, one message on the parameter's channel for a plugin that asked
/// for channels, and otherwise the same message on each of the 16
/// channels, in channel order.
fn expected_messages(set: MidiControllers, id: u32, value: i32) -> Vec<MidiMessage> {
    let Some((_, channel, controller)) = expected_hidden(set)
        .into_iter()
        .find(|&(hidden, ..)| hidden == id)
    else {
        return Vec::new();
    };
    match channel {
        Some(channel) => vec![message(channel as u8, controller, value)],
        None => (0..16)
            .map(|channel| message(channel, controller, value))
            .collect(),
    }
}

/// `message` on frame `offset` on each of the 16 channels, in channel
/// order, as a controller shared by every channel is heard.
fn everywhere(offset: u32, message: MidiMessage) -> Vec<(u32, MidiMessage)> {
    let controller = MidiController::of(&message).unwrap().1;
    let value = match message {
        MidiMessage::ControlChange { value, .. } => i32::from(value),
        MidiMessage::ChannelPressure { pressure, .. } => i32::from(pressure),
        MidiMessage::PitchBend { value, .. } => i32::from(value),
        other => panic!("not a controller: {other:?}"),
    };
    (0..16)
        .map(|channel| (offset, self::message(channel, controller, value)))
        .collect()
}

fn expected_title(channel: Option<u32>, controller: MidiController) -> String {
    let name = match controller.control_number() {
        Some(number) => format!("CC {number}"),
        None if controller == MidiController::PITCH_BEND => "Pitch Bend".to_owned(),
        None => "Pressure".to_owned(),
    };
    match channel {
        Some(channel) => format!("MIDI {} {name}", channel + 1),
        None => format!("MIDI {name}"),
    }
}

/// A probe of declaration `D`, created through the factory and activated
/// for blocks of up to 32 frames, with an empty notebook that has room.
fn probe<D: Declaration>() -> Host {
    let host = Host::new::<Probe<D>>(plugin_factory::<Probe<D>>());
    host.activate(48_000.0, 32);
    let mut heard = D::heard().lock().unwrap();
    heard.clear();
    heard.reserve(4_096);
    host
}

/// What the probe has heard since it was last asked.
fn heard<D: Declaration>() -> Vec<(u32, MidiMessage)> {
    let mut heard = D::heard().lock().unwrap();
    let copy = heard.clone();
    // Cleared, not taken: the room stays for the next block.
    heard.clear();
    copy
}

fn mapping_of(host: &Host, bus: i32, channel: i16, controller: i16) -> Option<u32> {
    let mapping: ComPtr<IMidiMapping> = host.component.cast().unwrap();
    let mut id = 0xDEAD_BEEF;
    let result = unsafe { mapping.getMidiControllerAssignment(bus, channel, controller, &mut id) };
    if result == kResultOk {
        return Some(id);
    }
    assert_eq!(result, kResultFalse);
    assert_eq!(id, 0xDEAD_BEEF, "a refused query wrote its answer");
    None
}

/// Checks the parameter list, units and resting values of a probe of
/// declaration `D`; returns how many hidden parameters it has.
fn check_listing<D: Declaration>() -> usize {
    let host = probe::<D>();
    let set = declared::<D>();
    let hidden = expected_hidden(set);
    let units: ComPtr<IUnitInfo> = host.component.cast().unwrap();
    unsafe {
        assert_eq!(
            host.controller.getParameterCount() as usize,
            1 + hidden.len(),
            "{}",
            D::ID
        );
        assert_eq!(units.getUnitCount(), 1 + i32::from(!hidden.is_empty()));
    }
    let own = host.param_info(0).unwrap();
    assert_eq!(wide(&own.title), "Gain");
    assert_eq!(own.unitId, vst::kRootUnitId);

    for (position, &(id, channel, controller)) in hidden.iter().enumerate() {
        let info = host.param_info(1 + position as i32).unwrap();
        assert_eq!(info.id, id, "{} at {position}", D::ID);
        assert_eq!(wide(&info.title), expected_title(channel, controller));
        assert_eq!(wide(&info.shortTitle), expected_title(channel, controller));
        assert_eq!(wide(&info.units), "");
        assert_eq!(info.stepCount, 0);
        assert_eq!(
            info.flags,
            vst::ParameterInfo_::ParameterFlags_::kIsHidden as i32
        );
        assert_eq!(info.unitId, 1);
        let rest = if controller == MidiController::PITCH_BEND {
            0.5
        } else {
            0.0
        };
        assert_eq!(info.defaultNormalizedValue, rest);
        unsafe {
            assert_eq!(host.controller.getParamNormalized(id), rest);
            // The host may set one; the move reaches the plugin through
            // `process`, so nothing is stored.
            assert_eq!(host.controller.setParamNormalized(id, 0.3), kResultOk);
            assert_eq!(host.controller.getParamNormalized(id), rest);
            assert_eq!(
                host.controller.setParamNormalized(id, f64::NAN),
                kInvalidArgument
            );
        }
    }
    let count = 1 + hidden.len() as i32;
    for index in [count, count + 1, count + 2_080, i32::MAX, -1, i32::MIN] {
        assert!(host.param_info(index).is_none(), "{} index {index}", D::ID);
    }

    // Every other id of the reserved range is no parameter at all.
    for offset in (0..130 * 16).step_by(SPARSE) {
        let id = MIDI_BASE + offset;
        if hidden.iter().any(|&(listed, ..)| listed == id) {
            continue;
        }
        unsafe {
            assert_eq!(host.controller.getParamNormalized(id), 0.0, "{id:#x}");
            assert_eq!(
                host.controller.setParamNormalized(id, 0.5),
                kInvalidArgument,
                "{} accepted {id:#x}",
                D::ID
            );
        }
    }
    hidden.len()
}

#[test]
fn the_parameter_count_is_the_number_of_declared_controllers() {
    let _turn = PROBES.lock().unwrap_or_else(|poison| poison.into_inner());
    assert_eq!(check_listing::<Musical>(), 13);
    assert_eq!(check_listing::<Nothing>(), 0);
    assert_eq!(check_listing::<Listed>(), 3);
    assert_eq!(check_listing::<Channels>(), 13 * 16);
    assert_eq!(check_listing::<Shared>(), 130);
    assert_eq!(check_listing::<Everything>(), 2_080);
    assert_eq!(check_listing::<EffectWithAll>(), 0);
}

/// Asks for every controller on every channel of several buses, and
/// checks each answer. Returns how many questions were asked.
fn check_mapping<D: Declaration>() -> usize {
    let host = probe::<D>();
    let set = declared::<D>();
    let listed: Vec<u32> = expected_hidden(set).iter().map(|&(id, ..)| id).collect();
    let mut reached = vec![false; listed.len()];
    let channels = (-1..=17).chain([i16::MIN, i16::MAX]);
    let controllers: Vec<i16> = if cfg!(miri) {
        vec![-1, 0, 1, 64, 127, 128, 129, 130, i16::MAX]
    } else {
        (-1..=140).chain([i16::MIN, i16::MAX]).collect()
    };
    let mut asked = 0;
    for bus in [0, 1, -1, i32::MAX, i32::MIN] {
        for channel in channels.clone() {
            for &controller in &controllers {
                asked += 1;
                let answer = mapping_of(&host, bus, channel, controller);
                assert_eq!(
                    answer,
                    expected_mapping(set, bus, channel, controller),
                    "{} bus {bus} channel {channel} controller {controller}",
                    D::ID
                );
                if let Some(id) = answer {
                    let at = listed.iter().position(|&listed| listed == id);
                    let at = at.expect("mapped to a parameter that is not listed");
                    reached[at] = true;
                }
            }
        }
    }
    if !cfg!(miri) {
        assert!(reached.iter().all(|&reached| reached), "{}", D::ID);
    }

    // No answer without somewhere to put it.
    let mapping: ComPtr<IMidiMapping> = host.component.cast().unwrap();
    for controller in [0, 64, 128, 129] {
        let result =
            unsafe { mapping.getMidiControllerAssignment(0, 0, controller, ptr::null_mut()) };
        assert_eq!(result, kResultFalse);
    }
    asked
}

#[test]
fn every_channel_and_controller_is_mapped_as_declared() {
    let _turn = PROBES.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut asked = 0;
    asked += check_mapping::<Musical>();
    asked += check_mapping::<Nothing>();
    asked += check_mapping::<Listed>();
    asked += check_mapping::<Channels>();
    asked += check_mapping::<Shared>();
    asked += check_mapping::<Everything>();
    asked += check_mapping::<EffectWithAll>();
    if !cfg!(miri) {
        assert_eq!(asked, 7 * 5 * 21 * 144);
    }

    // Shared: one parameter, the same for all sixteen channels.
    let host = probe::<Musical>();
    let ids: Vec<_> = (0..16)
        .map(|channel| mapping_of(&host, 0, channel, 64))
        .collect();
    assert_eq!(ids, vec![Some(MIDI_BASE + 64); 16]);
    drop(host);
    // Per channel: sixteen.
    let host = probe::<Channels>();
    let ids: Vec<_> = (0..16)
        .map(|channel| mapping_of(&host, 0, channel, 64))
        .collect();
    let apart: Vec<_> = (0..16)
        .map(|channel| Some(MIDI_BASE + channel * 130 + 64))
        .collect();
    assert_eq!(ids, apart);
}

#[test]
fn controller_moves_arrive_as_midi_on_their_frames() {
    let _turn = PROBES.lock().unwrap_or_else(|poison| poison.into_inner());
    let host = probe::<Musical>();
    // The host asks on whatever channel the message came in on.
    let bend = mapping_of(&host, 0, 5, 129).unwrap();
    let pedal = mapping_of(&host, 0, 0, 64).unwrap();
    let volume = mapping_of(&host, 0, 15, 7).unwrap();
    let pressure = mapping_of(&host, 0, 3, 128).unwrap();
    let reset = mapping_of(&host, 0, 9, 121).unwrap();
    let changes = vec![
        (bend, vec![(3, 1.0), (10, 0.5), (30, 0.0)]),
        // A value that is not a number is dropped; out of range is clamped.
        (pedal, vec![(0, 2.0), (8, f64::NAN), (31, -1.0)]),
        (volume, vec![(7, 0.5)]),
        (pressure, vec![(12, 0.25)]),
        (reset, vec![(20, 0.0)]),
        // Not declared: CC 74, and the pedal's id for channel 2, which a
        // probe that shares its controllers does not have.
        (MIDI_BASE + 74, vec![(5, 1.0)]),
        (MIDI_BASE + 130 + 64, vec![(6, 1.0)]),
    ];
    let processed = host.process(&mut [vec![0.0f32; 32]], 32, None, changes, vec![], None);
    assert_eq!(processed.status, kResultOk);
    assert_eq!(processed.heap_calls, 0, "controller moves used the heap");
    let cc = |controller, value| MidiMessage::ControlChange {
        channel: 0,
        controller,
        value,
    };
    // The probe shares its controllers: the host's channel is lost, so
    // each move is heard on every channel, channel by channel, on its
    // frame.
    let moves = [
        (0, cc(64, 127)),
        (
            3,
            MidiMessage::PitchBend {
                channel: 0,
                value: 8_191,
            },
        ),
        (7, cc(7, 64)),
        (
            10,
            MidiMessage::PitchBend {
                channel: 0,
                value: 0,
            },
        ),
        (
            12,
            MidiMessage::ChannelPressure {
                channel: 0,
                pressure: 32,
            },
        ),
        (20, cc(121, 0)),
        (
            30,
            MidiMessage::PitchBend {
                channel: 0,
                value: -8_192,
            },
        ),
        (31, cc(64, 0)),
    ];
    let got = heard::<Musical>();
    assert_eq!(got.len(), 8 * 16);
    assert_eq!(
        got,
        moves
            .iter()
            .flat_map(|&(offset, message)| everywhere(offset, message))
            .collect::<Vec<_>>()
    );
    // Spelled out for one of them: the volume, on the tenth channel.
    assert_eq!(
        got[2 * 16 + 9],
        (
            7,
            MidiMessage::ControlChange {
                channel: 9,
                controller: 7,
                value: 64
            }
        )
    );
    drop(host);

    // Per channel: each keeps its own.
    let host = probe::<Channels>();
    let pedal_9 = mapping_of(&host, 0, 9, 64).unwrap();
    let bend_2 = mapping_of(&host, 0, 2, 129).unwrap();
    let bend_15 = mapping_of(&host, 0, 15, 129).unwrap();
    let changes = vec![
        (pedal_9, vec![(4, 1.0)]),
        (bend_2, vec![(1, 0.75)]),
        (bend_15, vec![(1, 0.25)]),
        // Channel 2's CC 74 is not declared.
        (MIDI_BASE + 2 * 130 + 74, vec![(2, 1.0)]),
    ];
    let processed = host.process(&mut [vec![0.0f32; 8]], 8, None, changes, vec![], None);
    assert_eq!(processed.heap_calls, 0);
    assert_eq!(
        heard::<Channels>(),
        vec![
            (
                1,
                MidiMessage::PitchBend {
                    channel: 2,
                    value: 4_095
                }
            ),
            (
                1,
                MidiMessage::PitchBend {
                    channel: 15,
                    value: -4_096
                }
            ),
            (
                4,
                MidiMessage::ControlChange {
                    channel: 9,
                    controller: 64,
                    value: 127
                }
            ),
        ]
    );
    drop(host);

    // A plugin that declares nothing hears nothing, whatever the host
    // sends.
    let host = probe::<Nothing>();
    let changes = (0..130 * 16)
        .step_by(7)
        .map(|offset| (MIDI_BASE + offset, vec![(0, 1.0)]))
        .collect();
    let processed = host.process(&mut [vec![0.0f32; 8]], 8, None, changes, vec![], None);
    assert_eq!(processed.status, kResultOk);
    assert_eq!(processed.heap_calls, 0);
    assert_eq!(heard::<Nothing>(), vec![]);
}

#[test]
fn controller_positions_become_exact_midi_values() {
    let _turn = PROBES.lock().unwrap_or_else(|poison| poison.into_inner());
    // Per channel, so each move is heard once, on channel 3.
    let host = probe::<Channels>();
    let modulation = mapping_of(&host, 0, 3, 1).unwrap();
    let pressure = mapping_of(&host, 0, 3, 128).unwrap();
    let bend = mapping_of(&host, 0, 3, 129).unwrap();

    // Each table is the position the host sends and the MIDI value that
    // must arrive, worked out by hand: the nearest step, a half step going
    // up, and the ends for anything past them.
    let seven_bits: [(f64, Option<i32>); 16] = [
        (0.0, Some(0)),
        (0.003, Some(0)),
        (0.004, Some(1)),
        (1.0 / 127.0, Some(1)),
        (0.25, Some(32)),
        (0.4999, Some(63)),
        (0.5, Some(64)),
        (64.0 / 127.0, Some(64)),
        (0.996, Some(126)),
        (0.997, Some(127)),
        (1.0, Some(127)),
        (1.5, Some(127)),
        (-0.25, Some(0)),
        (f64::INFINITY, Some(127)),
        (f64::NEG_INFINITY, Some(0)),
        (f64::NAN, None),
    ];
    let fourteen_bits: [(f64, Option<i32>); 13] = [
        (0.0, Some(-8_192)),
        (0.000_03, Some(-8_192)),
        (0.000_04, Some(-8_191)),
        (1.0 / 16_383.0, Some(-8_191)),
        (0.25, Some(-4_096)),
        (8_191.0 / 16_383.0, Some(-1)),
        (0.5, Some(0)),
        (8_192.0 / 16_383.0, Some(0)),
        (0.75, Some(4_095)),
        (1.0, Some(8_191)),
        (2.0, Some(8_191)),
        (-1.0, Some(-8_192)),
        (f64::NAN, None),
    ];
    /// A parameter, its table, and the message a MIDI value makes.
    type Case<'a> = (u32, &'a [(f64, Option<i32>)], fn(i32) -> MidiMessage);
    let cases: [Case<'_>; 3] = [
        (modulation, &seven_bits, |value| {
            MidiMessage::ControlChange {
                channel: 3,
                controller: 1,
                value: value as u8,
            }
        }),
        (pressure, &seven_bits, |value| {
            MidiMessage::ChannelPressure {
                channel: 3,
                pressure: value as u8,
            }
        }),
        (bend, &fourteen_bits, |value| MidiMessage::PitchBend {
            channel: 3,
            value: value as i16,
        }),
    ];
    for (id, table, expected) in cases {
        let points: Vec<(i32, f64)> = table
            .iter()
            .enumerate()
            .map(|(frame, &(position, _))| (frame as i32, position))
            .collect();
        let processed = host.process(
            &mut [vec![0.0f32; 32]],
            32,
            None,
            vec![(id, points)],
            vec![],
            None,
        );
        assert_eq!(processed.status, kResultOk);
        assert_eq!(processed.heap_calls, 0);
        let wanted: Vec<(u32, MidiMessage)> = table
            .iter()
            .enumerate()
            .filter_map(|(frame, &(_, midi))| Some((frame as u32, expected(midi?))))
            .collect();
        assert_eq!(heard::<Channels>(), wanted, "{id:#x}");
    }
}

// --- A `Synth` instrument on every MIDI channel ------------------------------

/// A `Synth` instrument, as `plugins/pluck` is, with the controllers of
/// declaration `D`. Its voices play their key number, moved by the wheel.
struct Ensemble<D> {
    synth: Synth<KeyVoice>,
    params: Arc<ProbeParams>,
    declaration: PhantomData<fn() -> D>,
}

impl<D: Declaration> Plugin for Ensemble<D> {
    const INFO: PluginInfo = PluginInfo {
        id: D::ID,
        name: "Ensemble",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays key numbers on every channel",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 0,
        outputs: 1,
        sidechain: 0,
    };
    const MIDI_CONTROLLERS: MidiControllers = D::CONTROLLERS;
    type Params = ProbeParams;

    fn new() -> Self {
        Self {
            synth: Synth::with_voices(4, KeyVoice::default),
            params: Arc::new(ProbeParams {
                gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5),
            }),
            declaration: PhantomData,
        }
    }
    fn params(&self) -> Arc<ProbeParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.synth.prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.synth.process_with(buffer, context);
    }
}

fn ensemble<D: Declaration>() -> Host {
    let host = Host::new::<Ensemble<D>>(plugin_factory::<Ensemble<D>>());
    host.activate(48_000.0, 32);
    host
}

/// A note on or off on `channel`.
fn note_on_channel(offset: i32, on: bool, channel: i16, pitch: i16) -> vst::Event {
    use vst::Event_::EventTypes_ as types;
    if on {
        let mut event = event(offset, types::kNoteOnEvent as u32);
        event.__field0.noteOn = vst::NoteOnEvent {
            channel,
            pitch,
            tuning: 0.0,
            velocity: 1.0,
            length: 0,
            noteId: -1,
        };
        event
    } else {
        let mut event = event(offset, types::kNoteOffEvent as u32);
        event.__field0.noteOff = vst::NoteOffEvent {
            channel,
            pitch,
            velocity: 0.0,
            noteId: -1,
            tuning: 0.0,
        };
        event
    }
}

/// Plays one block of eight frames and returns it, checking that the
/// plugin kept off the heap.
fn play(host: &Host, changes: Vec<(u32, Vec<(i32, f64)>)>, events: Vec<vst::Event>) -> Vec<f32> {
    let processed = host.process(&mut [vec![0.0f32; 8]], 8, None, changes, events, None);
    assert_eq!(processed.status, kResultOk);
    assert_eq!(processed.heap_calls, 0, "the block used the heap");
    processed.output[0].clone()
}

/// Asserts that every sample of `out` is within a thousandth of `want`,
/// which pitch bend needs: all the way up is two semitones less the one
/// step in 8,192 that MIDI cannot reach.
fn assert_near(out: &[f32], want: &[f32], what: &str) {
    assert_eq!(out.len(), want.len());
    for (&sample, &wanted) in out.iter().zip(want) {
        assert!((sample - wanted).abs() < 0.001, "{what}: {out:?}");
    }
}

const WHEEL: i16 = 129;
const PEDAL: i16 = 64;
const RESET_ALL_CONTROLLERS: i16 = 121;
const ALL_NOTES_OFF: i16 = 123;

/// A keyboard set to `channel` plays a note and uses the wheel, the pedal,
/// "reset all controllers" and "all notes off", as the host's MIDI
/// mapping routes them, and the note on that channel follows each.
fn one_channel_is_played_with_its_controllers<D: Declaration>(channel: i16) {
    let host = ensemble::<D>();
    let what = format!("{} channel {channel}", D::ID);
    let wheel = mapping_of(&host, 0, channel, WHEEL).unwrap();
    let pedal = mapping_of(&host, 0, channel, PEDAL).unwrap();
    let reset = mapping_of(&host, 0, channel, RESET_ALL_CONTROLLERS).unwrap();
    let off = mapping_of(&host, 0, channel, ALL_NOTES_OFF).unwrap();

    // The wheel all the way up bends the note two semitones.
    let out = play(
        &host,
        vec![(wheel, vec![(4, 1.0)])],
        vec![note_on_channel(0, true, channel, 60)],
    );
    assert_near(
        &out,
        &[60.0, 60.0, 60.0, 60.0, 62.0, 62.0, 62.0, 62.0],
        &what,
    );
    // "Reset all controllers" puts the wheel back.
    let out = play(&host, vec![(reset, vec![(2, 0.0)])], vec![]);
    assert_near(
        &out,
        &[62.0, 62.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0],
        &what,
    );
    // The pedal holds the note after its key is let go...
    let out = play(
        &host,
        vec![(pedal, vec![(2, 1.0)])],
        vec![note_on_channel(4, false, channel, 60)],
    );
    assert_eq!(out, [60.0; 8], "{what}: the pedal did not hold the note");
    // ...until it is lifted.
    let out = play(&host, vec![(pedal, vec![(4, 0.0)])], vec![]);
    assert_eq!(out, [60.0, 60.0, 60.0, 60.0, 0.0, 0.0, 0.0, 0.0], "{what}");
    // "All notes off" releases a note whose key is still down.
    let out = play(
        &host,
        vec![(off, vec![(4, 0.0)])],
        vec![note_on_channel(0, true, channel, 64)],
    );
    assert_eq!(
        out,
        [64.0, 64.0, 64.0, 64.0, 0.0, 0.0, 0.0, 0.0],
        "{what}: all notes off left the note sounding"
    );
}

#[test]
fn a_synth_on_any_channel_follows_that_channels_controllers() {
    // Shared, the default every existing instrument gets, and per channel.
    // Miri interprets every frame, so it plays the first and the last.
    let channels: &[i16] = if cfg!(miri) {
        &[0, 15]
    } else {
        &[0, 1, 4, 9, 15]
    };
    for &channel in channels {
        one_channel_is_played_with_its_controllers::<Musical>(channel);
        one_channel_is_played_with_its_controllers::<Channels>(channel);
    }
}

#[test]
fn a_synth_with_shared_controllers_applies_each_to_every_channel() {
    // Notes on channels 3 and 8, and the wheel of channel 8, as the host
    // maps it: shared, it bends both notes. This is the price of sharing,
    // and what the documentation says.
    let host = ensemble::<Musical>();
    let wheel = mapping_of(&host, 0, 7, WHEEL).unwrap();
    let out = play(
        &host,
        vec![(wheel, vec![(4, 1.0)])],
        vec![
            note_on_channel(0, true, 2, 60),
            note_on_channel(0, true, 7, 64),
        ],
    );
    assert_near(
        &out,
        &[124.0, 124.0, 124.0, 124.0, 128.0, 128.0, 128.0, 128.0],
        "shared wheel",
    );
    drop(host);

    // The pedal of channel 10 holds a note on channel 1.
    let host = ensemble::<Musical>();
    let pedal = mapping_of(&host, 0, 9, PEDAL).unwrap();
    let out = play(
        &host,
        vec![(pedal, vec![(2, 1.0)])],
        vec![
            note_on_channel(0, true, 0, 60),
            note_on_channel(4, false, 0, 60),
        ],
    );
    assert_eq!(out, [60.0; 8], "shared pedal");
    drop(host);

    // Per channel, the same moves reach their own channel alone.
    let host = ensemble::<Channels>();
    let wheel = mapping_of(&host, 0, 7, WHEEL).unwrap();
    let out = play(
        &host,
        vec![(wheel, vec![(4, 1.0)])],
        vec![
            note_on_channel(0, true, 2, 60),
            note_on_channel(0, true, 7, 64),
        ],
    );
    assert_near(
        &out,
        &[124.0, 124.0, 124.0, 124.0, 126.0, 126.0, 126.0, 126.0],
        "own wheel",
    );
    drop(host);

    let host = ensemble::<Channels>();
    let pedal = mapping_of(&host, 0, 9, PEDAL).unwrap();
    let out = play(
        &host,
        vec![(pedal, vec![(2, 1.0)])],
        vec![
            note_on_channel(0, true, 0, 60),
            note_on_channel(4, false, 0, 60),
        ],
    );
    assert_eq!(
        out,
        [60.0, 60.0, 60.0, 60.0, 0.0, 0.0, 0.0, 0.0],
        "own pedal"
    );
}

#[test]
fn a_shared_controller_moved_while_stopped_reaches_every_channel() {
    let host = ensemble::<Musical>();
    let wheel = mapping_of(&host, 0, 12, WHEEL).unwrap();
    // The wheel all the way down while the host is stopped.
    let processed = host.process(
        &mut [vec![0.0f32; 8]],
        0,
        None,
        vec![(wheel, vec![(0, 0.0)])],
        vec![],
        None,
    );
    assert_eq!(processed.status, kResultOk);
    assert_eq!(processed.heap_calls, 0);
    // Then notes on two channels, both bent down two semitones.
    let out = play(
        &host,
        vec![],
        vec![
            note_on_channel(0, true, 12, 60),
            note_on_channel(0, true, 5, 70),
        ],
    );
    assert_eq!(out, [126.0; 8]);
}

#[test]
fn controllers_leave_the_state_alone() {
    let _turn = PROBES.lock().unwrap_or_else(|poison| poison.into_inner());
    let host = probe::<Everything>();
    let before = host.save().unwrap();
    // Every controller on every channel, moved by the host on both the
    // controller and the processor.
    let changes: Vec<_> = expected_hidden(MidiControllers::ALL.per_channel())
        .iter()
        .map(|&(id, ..)| (id, vec![(0, 0.75)]))
        .collect();
    for &(id, _) in &changes {
        unsafe { assert_eq!(host.controller.setParamNormalized(id, 0.75), kResultOk) };
    }
    let processed = host.process(&mut [vec![0.0f32; 4]], 4, None, changes, vec![], None);
    assert_eq!(processed.heap_calls, 0);
    assert_eq!(heard::<Everything>().len(), 2_080);

    let after = host.save().unwrap();
    assert_eq!(after, before, "a controller move changed the state");
    let text = String::from_utf8(after).unwrap();
    assert!(!text.contains("MIDI"), "{text}");
    unsafe {
        assert_eq!(
            host.controller
                .getParamNormalized(param_id(ParamId::from_name("gain"))),
            0.5
        );
    }
    // And the state loads back.
    assert_eq!(host.load(text.as_bytes()), kResultOk);
}

/// A small, fixed sequence of pseudo-random numbers.
struct Dice(u64);

impl Dice {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, limit: u64) -> u64 {
        self.next() % limit
    }
}

/// Plays a probe of declaration `D` a long run of random blocks and host
/// calls, and checks every block against what the declaration says it
/// should hear. Returns how many points were sent.
fn fuzz<D: Declaration>(seed: u64) -> usize {
    let host = probe::<D>();
    let set = declared::<D>();
    let hidden = expected_hidden(set);
    let mut dice = Dice(seed);
    // Moves from blocks with no audio, waiting for one that has some.
    let mut waiting: Vec<MidiMessage> = Vec::new();
    let mut sent = 0;
    for round in 0..ROUNDS {
        // A few calls on the main thread in between.
        for _ in 0..dice.below(4) {
            let channel = dice.below(20) as i16 - 2;
            let controller = dice.below(140) as i16 - 2;
            let bus = dice.below(3) as i32 - 1;
            assert_eq!(
                mapping_of(&host, bus, channel, controller),
                expected_mapping(set, bus, channel, controller)
            );
            let id = MIDI_BASE + dice.below(2_100) as u32;
            let listed = hidden.iter().any(|&(listed, ..)| listed == id);
            let result = unsafe { host.controller.setParamNormalized(id, 0.5) };
            assert_eq!(result == kResultOk, listed, "{id:#x}");
        }

        let frames = if dice.below(5) == 0 {
            0
        } else {
            1 + dice.below(32) as usize
        };
        let mut changes = Vec::new();
        let mut expected: Vec<(u32, MidiMessage)> = Vec::new();
        for _ in 0..dice.below(6) {
            let id = match dice.below(4) {
                0 if !hidden.is_empty() => hidden[dice.below(hidden.len() as u64) as usize].0,
                1 => MIDI_BASE + dice.below(2_080) as u32,
                2 => dice.next() as u32,
                _ => MIDI_BASE + dice.below(130) as u32,
            };
            // The MIDI values this id's controller moves through. Pitch
            // bend has 14 bits; everything else, and an id that is no
            // controller at all, 7.
            let is_bend = id.checked_sub(MIDI_BASE).is_some_and(|at| at % 130 == 129);
            let (lowest, steps) = if is_bend { (-8_192, 16_383) } else { (0, 127) };
            let mut points = Vec::new();
            for _ in 0..1 + dice.below(3) {
                let offset = dice.below(36) as i32 - 2;
                // Positions on the controller's own steps, whose MIDI value
                // is known without working anything out, and positions
                // past either end, which stop there. The rounding between
                // steps is pinned by
                // `controller_positions_become_exact_midi_values`.
                let (value, midi) = match dice.below(10) {
                    0 => (f64::NAN, None),
                    1 => (f64::INFINITY, Some(lowest + steps)),
                    2 => (f64::NEG_INFINITY, Some(lowest)),
                    3 => (1.0 + dice.below(1_000) as f64 / 100.0, Some(lowest + steps)),
                    4 => (-((1 + dice.below(1_000)) as f64) / 100.0, Some(lowest)),
                    _ => {
                        let step = dice.below(steps as u64 + 1) as i32;
                        (f64::from(step) / f64::from(steps), Some(lowest + step))
                    }
                };
                points.push((offset, value));
                if let Some(midi) = midi {
                    for message in expected_messages(set, id, midi) {
                        expected.push((offset.max(0) as u32, message));
                    }
                }
            }
            sent += points.len();
            changes.push((id, points));
        }
        let processed = host.process(&mut [vec![0.0f32; 32]], frames, None, changes, vec![], None);
        assert_eq!(processed.status, kResultOk);
        assert_eq!(processed.heap_calls, 0, "{} round {round}", D::ID);
        // Points of the same frame keep the order they were sent in.
        expected.sort_by_key(|&(offset, _)| offset);
        if frames == 0 {
            // Held for the next block with audio, on its first frame.
            waiting.extend(expected.into_iter().map(|(_, message)| message));
            assert_eq!(heard::<D>(), vec![], "{} round {round}", D::ID);
            continue;
        }
        let mut whole: Vec<(u32, MidiMessage)> =
            waiting.drain(..).map(|message| (0, message)).collect();
        whole.extend(expected);
        // Points of the same frame keep the order they were sent in.
        whole.sort_by_key(|&(offset, _)| offset);
        assert_eq!(heard::<D>(), whole, "{} round {round}", D::ID);
    }
    sent
}

#[test]
fn random_host_traffic_is_heard_exactly_as_declared() {
    let _turn = PROBES.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut sent = 0;
    sent += fuzz::<Musical>(0x9E37_79B9_7F4A_7C15);
    sent += fuzz::<Nothing>(0xD1B5_4A32_D192_ED03);
    sent += fuzz::<Listed>(0x8CB9_2BA7_2F3D_8DD7);
    sent += fuzz::<Channels>(0xABC9_8388_FB8F_AC03);
    sent += fuzz::<Shared>(0x2545_F491_4F6C_DD1D);
    sent += fuzz::<Everything>(0x94D0_49BB_1331_11EB);
    sent += fuzz::<EffectWithAll>(0xBF58_476D_1CE4_E5B9);
    assert!(sent > 7 * ROUNDS, "only {sent} points were sent");
}

// --- An MPE instrument ---------------------------------------------------------

/// An `Ensemble` made to be played from an MPE controller: a lower zone
/// from the start, and controllers kept per channel, as MPE needs.
struct MpeEnsemble {
    synth: Synth<KeyVoice>,
    params: Arc<ProbeParams>,
}

impl Plugin for MpeEnsemble {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.mpe",
        name: "MPE ensemble",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays key numbers, one note per channel",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 0,
        outputs: 1,
        sidechain: 0,
    };
    const MIDI_CONTROLLERS: MidiControllers = MidiControllers::MUSICAL.per_channel();
    const MPE: manguebyte_plugin::Mpe = manguebyte_plugin::Mpe::LOWER_ZONE;
    type Params = ProbeParams;

    fn new() -> Self {
        Self {
            synth: Synth::with_voices(4, KeyVoice::default),
            params: Arc::new(ProbeParams {
                gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5),
            }),
        }
    }
    fn params(&self) -> Arc<ProbeParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.synth.prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.synth.process_with(buffer, context);
    }
}

/// A note-on or note-off on `channel` with the host's identifier `id`.
fn note_with_id(offset: i32, on: bool, channel: i16, pitch: i16, id: i32) -> vst::Event {
    let mut event = note_on_channel(offset, on, channel, pitch);
    // `note_on_channel` wrote the member of the union that `on` names.
    if on {
        event.__field0.noteOn.noteId = id;
    } else {
        event.__field0.noteOff.noteId = id;
    }
    event
}

#[test]
fn an_mpe_instrument_bends_one_note_by_its_member_channel() {
    let host = Host::new::<MpeEnsemble>(plugin_factory::<MpeEnsemble>());
    host.activate(48_000.0, 32);
    // The wheel of member channel 3, as the host's MIDI mapping routes it.
    let wheel = mapping_of(&host, 0, 2, WHEEL).unwrap();
    // Notes on member channels 2 and 3 with the host's identifiers; the
    // wheel of channel 3 goes all the way up on frame 4.
    let out = play(
        &host,
        vec![(wheel, vec![(4, 1.0)])],
        vec![
            note_with_id(0, true, 1, 60, 10),
            note_with_id(0, true, 2, 64, 11),
        ],
    );
    // Up 48 semitones, the member channels' range, on the note of channel
    // 3 alone; the note of channel 2 is not moved.
    assert_eq!(
        out,
        [124.0, 124.0, 124.0, 124.0, 172.0, 172.0, 172.0, 172.0]
    );

    // The note-off on channel 3 ends that note only.
    let out = play(&host, vec![], vec![note_with_id(2, false, 2, 64, 11)]);
    assert_eq!(out, [172.0, 172.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0]);
}

// --- 64-bit processing ---------------------------------------------------------

/// Samples that a 32-bit float does not hold exactly.
fn signal(frames: usize) -> Vec<f64> {
    (0..frames)
        .map(|frame| (frame as f64 * 0.017).sin() * 0.9 + 1e-9)
        .collect()
}

impl Host {
    /// Sets the component up for `size` samples and activates it.
    fn activate_with(&self, size: vst::SymbolicSampleSizes, max_block: i32) {
        let mut setup = vst::ProcessSetup {
            processMode: vst::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: size as i32,
            maxSamplesPerBlock: max_block,
            sampleRate: 48_000.0,
        };
        unsafe {
            assert_eq!(self.processor.setupProcessing(&mut setup), kResultOk);
            assert_eq!(self.component.setActive(1), kResultOk);
            assert_eq!(self.processor.setProcessing(1), kResultOk);
        }
    }

    /// Processes one 64-bit block in place on a main bus of `main`, with
    /// `changes` as automation points per parameter id. Returns the status
    /// and the heap calls made.
    fn process_wide(
        &self,
        main: &mut [Vec<f64>],
        changes: Vec<(u32, Vec<(i32, f64)>)>,
    ) -> (tresult, usize) {
        let frames = main[0].len();
        let mut channels: Vec<*mut f64> = main.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let bus = vst::AudioBusBuffers {
            numChannels: channels.len() as i32,
            silenceFlags: 0,
            __field0: vst::AudioBusBuffers__type0 {
                channelBuffers64: channels.as_mut_ptr(),
            },
        };
        let mut inputs = [bus];
        let mut outputs = [bus];
        let changes = ComWrapper::new(Changes {
            queues: changes
                .into_iter()
                .map(|(id, points)| ComWrapper::new(Queue { id, points }))
                .collect(),
        });
        let mut data = vst::ProcessData {
            processMode: vst::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: vst::SymbolicSampleSizes_::kSample64 as i32,
            numSamples: frames as i32,
            numInputs: 1,
            numOutputs: 1,
            inputs: inputs.as_mut_ptr(),
            outputs: outputs.as_mut_ptr(),
            inputParameterChanges: changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr(),
            outputParameterChanges: ptr::null_mut(),
            inputEvents: ptr::null_mut(),
            outputEvents: ptr::null_mut(),
            processContext: ptr::null_mut(),
        };
        HEAP_CALLS.with(|calls| calls.set(0));
        IN_PROCESS.with(|watching| watching.set(true));
        let status = unsafe { self.processor.process(&mut data) };
        IN_PROCESS.with(|watching| watching.set(false));
        (status, HEAP_CALLS.with(Cell::get))
    }
}

/// Plays `signal` through `Level` with its level automated, in blocks cut
/// as `pattern` says, in 64 bits or in 32, and returns the left channel
/// widened to 64 bits.
fn level_through(frames: usize, pattern: &[usize], wide: bool) -> Vec<f64> {
    let input = signal(frames);
    // The level goes to 2 on frame 10 and to 0.5 on frame 50.
    let points = [(10usize, 0.5), (50, 0.125)];
    let host = Host::new::<Level>(plugin_factory::<Level>());
    let size = if wide {
        vst::SymbolicSampleSizes_::kSample64
    } else {
        vst::SymbolicSampleSizes_::kSample32
    };
    host.activate_with(size, 128);
    let mut played = Vec::with_capacity(frames);
    let (mut start, mut next) = (0, 0);
    while start < frames {
        let length = pattern[next % pattern.len()].min(frames - start);
        next += 1;
        let end = start + length;
        let block: Vec<(i32, f64)> = points
            .iter()
            .filter(|(at, _)| (start..end).contains(at))
            .map(|&(at, value)| ((at - start) as i32, value))
            .collect();
        let changes = vec![(param_id(LEVEL), block)];
        let piece = &input[start..end];
        if wide {
            let mut main = vec![piece.to_vec(); 2];
            let (status, heap) = host.process_wide(&mut main, changes);
            assert_eq!((status, heap), (kResultOk, 0));
            played.extend_from_slice(&main[0]);
        } else {
            let mut main = vec![piece.iter().map(|&s| s as f32).collect::<Vec<f32>>(); 2];
            let processed = host.process(&mut main, length, None, changes, vec![], None);
            assert_eq!((processed.status, processed.heap_calls), (kResultOk, 0));
            played.extend(processed.output[0].iter().map(|&s| f64::from(s)));
        }
        start = end;
    }
    played
}

#[test]
fn sixty_four_bit_processing_is_the_32_bit_result_widened() {
    let _guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let narrow = level_through(96, &[96], false);
    let wide = level_through(96, &[96], true);
    // The plugin hears the input rounded to 32 bits, and its output is
    // widened exactly: the two are the same numbers.
    assert_eq!(wide, narrow);
    assert!(
        wide.iter()
            .all(|&sample| f64::from(sample as f32) == sample)
    );
    assert_eq!(wide[20], f64::from(signal(96)[20] as f32 * 2.0));
    // Whatever the blocks.
    for pattern in [&[7, 25, 64][..], &[1], &[50, 46], &[13]] {
        assert_eq!(
            level_through(96, pattern, true),
            narrow,
            "cut as {pattern:?}"
        );
    }
    // A size that is neither is refused.
    let host = Host::new::<Level>(plugin_factory::<Level>());
    let mut setup = vst::ProcessSetup {
        processMode: vst::ProcessModes_::kRealtime as i32,
        symbolicSampleSize: 3,
        maxSamplesPerBlock: 64,
        sampleRate: 48_000.0,
    };
    unsafe { assert_eq!(host.processor.setupProcessing(&mut setup), kResultFalse) };
}

// --- Bus arrangements ----------------------------------------------------------

/// The layouts `Shape` has been given, in order.
static SHAPE_LAYOUTS: Mutex<Vec<AudioLayout>> = Mutex::new(Vec::new());

struct ShapeParams {
    level: FloatParam,
}
param_set!(ShapeParams { level });

/// Scales its input by 1.5, and adds 1000 when it has a sidechain.
struct Shape {
    params: Arc<ShapeParams>,
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
        AudioLayout::STEREO_EFFECT,
        AudioLayout::MONO_EFFECT.with_sidechain(1),
        AudioLayout::MONO_EFFECT,
    ];
    type Params = ShapeParams;

    fn new() -> Self {
        Self {
            params: Arc::new(ShapeParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 2.0), 1.5),
            }),
        }
    }
    fn params(&self) -> Arc<ShapeParams> {
        Arc::clone(&self.params)
    }
    fn set_layout(&mut self, layout: AudioLayout) {
        SHAPE_LAYOUTS.lock().unwrap().push(layout);
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        let keyed = if context.sidechain.is_some() {
            1_000.0
        } else {
            0.0
        };
        let level = self.params.level.value();
        for channel in buffer.iter_channels_mut() {
            channel
                .iter_mut()
                .for_each(|sample| *sample = *sample * level + keyed);
        }
    }
}

static SHAPES: Mutex<()> = Mutex::new(());

impl Host {
    fn arrange(&self, inputs: &[u64], outputs: &[u64]) -> tresult {
        let (mut inputs, mut outputs) = (inputs.to_vec(), outputs.to_vec());
        unsafe {
            self.processor.setBusArrangements(
                inputs.as_mut_ptr(),
                inputs.len() as i32,
                outputs.as_mut_ptr(),
                outputs.len() as i32,
            )
        }
    }

    fn arrangement(&self, direction: u32, index: i32) -> Option<u64> {
        let mut arrangement = 0u64;
        unsafe {
            (self
                .processor
                .getBusArrangement(direction as _, index, &mut arrangement)
                == kResultOk)
                .then_some(arrangement)
        }
    }

    fn activate_bus(&self, index: i32, state: bool) -> tresult {
        unsafe {
            self.component
                .activateBus(AUDIO as _, INPUT as _, index, u8::from(state))
        }
    }
}

#[test]
fn bus_arrangements_follow_the_layouts_offered() {
    let _guard = SHAPES.lock().unwrap_or_else(|poison| poison.into_inner());
    SHAPE_LAYOUTS.lock().unwrap().clear();
    let host = Host::new::<Shape>(plugin_factory::<Shape>());
    let (mono, stereo_arr, empty) = (
        vst::SpeakerArr::kMono,
        vst::SpeakerArr::kStereo,
        vst::SpeakerArr::kEmpty,
    );
    assert_eq!(host.bus_count(AUDIO, INPUT), 2);
    assert_eq!(host.arrangement(INPUT, 1), Some(stereo_arr));

    // Mono with a mono sidechain is offered.
    assert_eq!(host.arrange(&[mono, mono], &[mono]), kResultOk);
    assert_eq!(host.arrangement(INPUT, 0), Some(mono));
    assert_eq!(host.arrangement(INPUT, 1), Some(mono));
    assert_eq!(host.arrangement(OUTPUT, 0), Some(mono));
    assert_eq!(host.bus_info(AUDIO, INPUT, 0).unwrap().channelCount, 1);
    // Mono with a stereo sidechain is not; nothing changes.
    assert_eq!(host.arrange(&[mono, stereo_arr], &[mono]), kResultFalse);
    assert_eq!(host.arrangement(INPUT, 1), Some(mono));
    // The bus count is fixed.
    assert_eq!(host.arrange(&[stereo_arr], &[stereo_arr]), kResultFalse);
    assert_eq!(
        host.arrange(&[stereo_arr, stereo_arr], &[stereo_arr, stereo_arr]),
        kResultFalse
    );
    // Stereo with no sidechain is an empty arrangement on its bus.
    assert_eq!(host.arrange(&[stereo_arr, empty], &[stereo_arr]), kResultOk);
    assert_eq!(host.arrangement(INPUT, 1), Some(empty));
    assert_eq!(host.bus_info(AUDIO, INPUT, 1).unwrap().channelCount, 0);

    host.activate(48_000.0, 64);
    assert_eq!(
        SHAPE_LAYOUTS.lock().unwrap().last(),
        Some(&AudioLayout::STEREO_EFFECT)
    );
    // While active only the arrangement in use is accepted.
    assert_eq!(host.arrange(&[mono, mono], &[mono]), kResultFalse);
    assert_eq!(host.arrange(&[stereo_arr, empty], &[stereo_arr]), kResultOk);
    let mut audio = stereo(1.0, 16);
    let mut side = stereo(1.0, 16);
    let processed = host.process(&mut audio, 16, Some(&mut side), vec![], vec![], None);
    assert_eq!((processed.status, processed.heap_calls), (kResultOk, 0));
    assert_eq!(processed.output, stereo(1.5, 16));
    unsafe {
        host.processor.setProcessing(0);
        host.component.setActive(0);
    }

    // Mono, from the next activation.
    assert_eq!(host.arrange(&[mono, empty], &[mono]), kResultOk);
    host.activate(48_000.0, 64);
    assert_eq!(
        SHAPE_LAYOUTS.lock().unwrap().last(),
        Some(&AudioLayout::MONO_EFFECT)
    );
    let mut audio = vec![vec![2.0; 16]];
    let processed = host.process(&mut audio, 16, None, vec![], vec![], None);
    assert_eq!((processed.status, processed.heap_calls), (kResultOk, 0));
    assert_eq!(processed.output, [vec![3.0; 16]]);
}

#[test]
fn switching_the_sidechain_bus_picks_the_layout_without_it() {
    let _guard = SHAPES.lock().unwrap_or_else(|poison| poison.into_inner());
    SHAPE_LAYOUTS.lock().unwrap().clear();
    let host = Host::new::<Shape>(plugin_factory::<Shape>());
    // The sidechain starts off, so the plugin runs without one, which it
    // offers.
    host.activate(48_000.0, 64);
    assert_eq!(
        SHAPE_LAYOUTS.lock().unwrap().last(),
        Some(&AudioLayout::STEREO_EFFECT)
    );
    let mut audio = stereo(1.0, 8);
    let mut side = stereo(1.0, 8);
    let processed = host.process(&mut audio, 8, Some(&mut side), vec![], vec![], None);
    assert_eq!(processed.output, stereo(1.5, 8));
    unsafe {
        host.processor.setProcessing(0);
        host.component.setActive(0);
    }

    // Switched on, it runs with it.
    assert_eq!(host.activate_bus(1, true), kResultOk);
    assert_eq!(host.activate_bus(2, true), kInvalidArgument);
    host.activate(48_000.0, 64);
    assert_eq!(SHAPE_LAYOUTS.lock().unwrap().last(), Some(&Shape::LAYOUT));
    let mut audio = stereo(1.0, 8);
    let processed = host.process(&mut audio, 8, Some(&mut side), vec![], vec![], None);
    assert_eq!((processed.status, processed.heap_calls), (kResultOk, 0));
    assert_eq!(processed.output, stereo(1_001.5, 8));
    // Not switched while active.
    assert_eq!(host.activate_bus(1, false), kResultOk);
    let mut audio = stereo(1.0, 8);
    let processed = host.process(&mut audio, 8, Some(&mut side), vec![], vec![], None);
    assert_eq!(processed.output, stereo(1_001.5, 8));
}

#[test]
fn tail_is_infinite_until_plugin_is_activated() {
    let _guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let host = Host::new::<Level>(plugin_factory::<Level>());
    unsafe {
        assert_eq!(
            host.processor.getTailSamples(),
            vst::kInfiniteTail,
            "before activation the host must not assume silence"
        );
    }
}

#[test]
fn the_tail_a_plugin_states_is_reported_to_host_post_activation() {
    let _guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let host = Host::new::<Level>(plugin_factory::<Level>());

    unsafe {
        // Initial value is infinite
        assert_eq!(
            host.processor.getTailSamples(),
            vst::kInfiniteTail,
            "before activation the host must not assume silence"
        );

        host.activate(48_000.0, 64);
        // getTailSamples should return the override value after activation
        assert_eq!(
            host.processor.getTailSamples(),
            480,
            "after activation the host sees the tail the plugin stated"
        )
    }
}

#[test]
fn a_plugin_that_states_no_tail_reports_an_infinite_one() {
    let _guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let host = Host::new::<Keys>(plugin_factory::<Keys>());

    host.activate(48_000.0, 64);
    unsafe {
        // a plugin that states no tail keeps the infinite default
        assert_eq!(
            host.processor.getTailSamples(),
            vst::kInfiniteTail,
            "after activation the host sees the tail the plugin stated"
        )
    }
}
