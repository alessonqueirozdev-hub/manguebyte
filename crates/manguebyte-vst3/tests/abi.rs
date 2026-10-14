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
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use manguebyte_core::{AtomicF64, AudioBuffer, NoDenormals, ParamId};
use manguebyte_dsp::event::{EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth, Voice, split_at_events};
use manguebyte_params::{BoolParam, ChoiceParam, FloatParam, FloatRange, param_set};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
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

        // The layout is fixed: the plugin's own is accepted, others are not.
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
        assert_eq!(host.processor.canProcessSampleSize(double), kResultFalse);
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
        inputParameterChanges: meddling
            .as_com_ref::<IParameterChanges>()
            .unwrap()
            .as_ptr(),
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
        assert_eq!(host.controller.setComponentHandler(ptr::null_mut()), kResultOk);
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

    // MIDI controllers arrive as parameters: one hidden parameter for each
    // of 130 controllers on each of 16 channels, after the plugin's own.
    let mapping: ComPtr<IMidiMapping> = host.component.cast().unwrap();
    let controller_id = |channel: i16, controller: u32| unsafe {
        let mut id = 0u32;
        (mapping.getMidiControllerAssignment(0, channel, controller as i16, &mut id) == kResultOk)
            .then_some(id)
    };
    let bend = controller_id(0, vst::ControllerNumbers_::kPitchBend as u32).unwrap();
    let sustain = controller_id(0, 64).unwrap();
    assert_ne!(bend, sustain);
    assert_ne!(bend, controller_id(1, 129).unwrap());
    assert!(controller_id(16, 64).is_none());
    assert!(controller_id(0, 130).is_none());
    unsafe {
        assert_eq!(host.controller.getParameterCount(), 1 + 130 * 16);
        // Pitch bend rests in the middle.
        assert_eq!(host.controller.getParamNormalized(bend), 0.5);
    }
    let hidden = host.param_info(1 + 64).unwrap();
    assert_eq!(hidden.id, sustain);
    assert_eq!(wide(&hidden.title), "MIDI 1 CC 64");
    assert_ne!(
        hidden.flags & vst::ParameterInfo_::ParameterFlags_::kIsHidden as i32,
        0
    );
    assert_eq!(
        wide(&host.param_info(1 + 129).unwrap().title),
        "MIDI 1 Pitch Bend"
    );
    assert_eq!(
        wide(&host.param_info(1 + 130 + 128).unwrap().title),
        "MIDI 2 Pressure"
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
