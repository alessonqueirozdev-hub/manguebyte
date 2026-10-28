//! Hosts VST 3 plugins written in this file, that do what the example
//! plugins never do.
//!
//! The framework's own plugins are one object, report nothing while they
//! process and answer every question politely. The plugin here is two
//! objects, a component and a separate controller that talk through
//! connection points and messages from the host. Its controller edits its
//! own parameters through the host's component handler and asks for
//! restarts; its processor writes parameter changes and notes back. It has
//! an editor that needs no real window, resizes, asks its frame for sizes
//! and can refuse any step of being opened. And a
//! script makes it misbehave: claim more classes and parameters than
//! exist, answer with numbers that are not numbers, refuse to start, fail
//! while processing, refuse to be set up or activated, have no controller.
//!
//! The plugin lives in this executable and is reached through
//! `Vst3Module::from_factory`. Its code runs under the same heap watcher as
//! the host's, and does not allocate while processing: it records what it
//! is sent in atomics. A non-zero count is therefore the host's doing.
//!
//! Every `unsafe` block in this file is one side or the other of the VST 3
//! interfaces, with pointers that are valid for the call, so the blocks are
//! not commented one by one.
#![cfg(feature = "vst3")]
#![allow(clippy::undocumented_unsafe_blocks)]
#![allow(non_snake_case)]
// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast needed on one platform is redundant on the other.
#![allow(clippy::unnecessary_cast)]

mod common;

use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, Processor, Transport};
use manguebyte_host::editor::{EditorRequest, EditorSize, ParentWindow, WindowApi};
use manguebyte_host::vst3::vst3::Steinberg::Vst::{
    self as vst, IAttributeList, IAttributeListTrait, IAudioProcessor, IAudioProcessorTrait,
    IComponent, IComponentHandler, IComponentHandler2, IComponentHandler2Trait,
    IComponentHandlerTrait, IComponentTrait, IConnectionPoint, IConnectionPointTrait,
    IEditController, IEditControllerTrait, IEventListTrait, IHostApplication,
    IHostApplicationTrait, IMessage, IMessageTrait, IMidiMapping, IMidiMappingTrait,
    IParamValueQueueTrait, IParameterChangesTrait,
};
use manguebyte_host::vst3::vst3::Steinberg::{
    FIDString, FUnknown, IBStream, IBStreamTrait, IPlugFrame, IPlugFrameTrait, IPlugView,
    IPlugViewContentScaleSupport, IPlugViewContentScaleSupportTrait, IPlugViewTrait,
    IPluginBaseTrait, IPluginFactory, IPluginFactoryTrait, TBool, TUID, ViewRect, kInvalidArgument,
    kNotImplemented, kResultFalse, kResultOk, kResultTrue, tresult,
};
use manguebyte_host::vst3::vst3::{Class, ComPtr, ComRef, ComWrapper, Interface};
use manguebyte_host::vst3::{
    ClassId, HostEvent, MAX_CLASSES, MAX_PARAMS, ProcessFailure, ProcessStatus, Vst3Error,
    Vst3Module, Vst3Plugin,
};

// --- The plugin's script and what it records -------------------------------

const COMPONENT: ClassId =
    ClassId::from_words([0x1111_1111, 0x2222_2222, 0x3333_3333, 0x4444_4444]);
const CONTROLLER: ClassId =
    ClassId::from_words([0x5555_5555, 0x6666_6666, 0x7777_7777, 0x8888_8888]);

/// The gain: normalized, times two.
const GAIN: u32 = 7;
/// A level the processor reports. Read only.
const METER: u32 = 8;
/// Where the controller maps MIDI controller 74.
const BRIGHT: u32 = 9;

/// Records written on the audio thread without allocating: a fixed number
/// of entries of four words, and a count.
struct Tape {
    entries: Box<[[AtomicU64; 4]]>,
    len: AtomicUsize,
}

impl Tape {
    fn new(capacity: usize) -> Self {
        Self {
            entries: (0..capacity).map(|_| Default::default()).collect(),
            len: AtomicUsize::new(0),
        }
    }

    fn push(&self, words: [u64; 4]) {
        let at = self.len.fetch_add(1, Relaxed);
        if let Some(entry) = self.entries.get(at) {
            for (slot, word) in entry.iter().zip(words) {
                slot.store(word, Relaxed);
            }
        }
    }

    fn take(&self) -> Vec<[u64; 4]> {
        let len = self.len.swap(0, Relaxed).min(self.entries.len());
        self.entries[..len]
            .iter()
            .map(|entry| entry.each_ref().map(|word| word.load(Relaxed)))
            .collect()
    }
}

/// How the plugin is to behave, and what it saw.
struct Script {
    // --- How to behave ---
    classes: AtomicI32,
    params: AtomicI32,
    has_controller: AtomicBool,
    fail_process: AtomicBool,
    refuse_start: AtomicBool,
    refuse_setup: AtomicBool,
    refuse_activation: AtomicBool,
    report: AtomicBool,
    nan_values: AtomicBool,
    /// Whether the controller makes an editor.
    has_editor: AtomicBool,
    /// Whether the editor may be resized.
    resizable: AtomicBool,
    /// The one call the editor refuses, if any: `"platform"`, `"setFrame"`,
    /// `"attached"`, `"getSize"`, `"onSize"` or `"scale"`.
    view_refuses: Mutex<Option<&'static str>>,
    /// A size the editor asks its frame for from inside `attached`.
    resize_when_attached: Mutex<Option<(i32, i32)>>,

    // --- What happened ---
    /// The life of the two objects, in order, on the thread it happened.
    log: Mutex<Vec<(String, ThreadId)>>,
    class_info_calls: AtomicUsize,
    param_info_calls: AtomicUsize,
    /// Points the processor was sent: id, offset, value bits, call number.
    points: Tape,
    /// Events the processor was sent: type, offset, note id, value bits.
    events: Tape,
    /// Calls of `process`, and the frames of each.
    calls: Tape,
    process_calls: AtomicUsize,
    /// The gain the component holds, normalized, as bits.
    gain: AtomicU64,
    /// What the controller was told each parameter is, last first.
    told: Mutex<Vec<(u32, f64)>>,
    /// What the controller is given as the component's state, and as its
    /// own.
    component_states: Mutex<Vec<Vec<u8>>>,
    controller_states: Mutex<Vec<Vec<u8>>>,
    /// The handler the controller was given.
    handler: Mutex<Option<ComPtr<IComponentHandler>>>,
    /// What came through the connection points.
    messages: Mutex<Vec<(String, i64)>>,
    /// The frame the editor was last given, kept after it was taken back.
    frame: Mutex<Option<ComPtr<IPlugFrame>>>,
    /// What `resizeView` answered from inside `attached`.
    resize_answer: Mutex<Option<tresult>>,
}

impl Script {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            classes: AtomicI32::new(2),
            params: AtomicI32::new(3),
            has_controller: AtomicBool::new(true),
            fail_process: AtomicBool::new(false),
            refuse_start: AtomicBool::new(false),
            refuse_setup: AtomicBool::new(false),
            refuse_activation: AtomicBool::new(false),
            report: AtomicBool::new(false),
            nan_values: AtomicBool::new(false),
            has_editor: AtomicBool::new(true),
            resizable: AtomicBool::new(true),
            view_refuses: Mutex::new(None),
            resize_when_attached: Mutex::new(None),
            log: Mutex::new(Vec::new()),
            class_info_calls: AtomicUsize::new(0),
            param_info_calls: AtomicUsize::new(0),
            points: Tape::new(4_096),
            events: Tape::new(1_024),
            calls: Tape::new(1_024),
            process_calls: AtomicUsize::new(0),
            gain: AtomicU64::new(0.5f64.to_bits()),
            told: Mutex::new(Vec::new()),
            component_states: Mutex::new(Vec::new()),
            controller_states: Mutex::new(Vec::new()),
            handler: Mutex::new(None),
            messages: Mutex::new(Vec::new()),
            frame: Mutex::new(None),
            resize_answer: Mutex::new(None),
        })
    }

    /// Whether the editor is to refuse `call`.
    fn refuses(&self, call: &str) -> bool {
        *self.view_refuses.lock().unwrap() == Some(call)
    }

    /// The entries of the log about the editor, in order.
    fn view_log(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|what| what.starts_with("view"))
            .collect()
    }

    fn note(&self, what: &str) {
        self.log
            .lock()
            .unwrap()
            .push((what.to_owned(), std::thread::current().id()));
    }

    fn log(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|(what, _)| what.clone())
            .collect()
    }

    fn thread_of(&self, what: &str) -> Option<ThreadId> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .find(|(seen, _)| seen == what)
            .map(|&(_, thread)| thread)
    }

    fn gain(&self) -> f64 {
        f64::from_bits(self.gain.load(Relaxed))
    }

    fn handler(&self) -> ComPtr<IComponentHandler> {
        self.handler.lock().unwrap().clone().expect("a handler")
    }

    /// The points sent since the last call, as `(id, offset, value, call)`.
    fn points(&self) -> Vec<(u32, i32, f64, usize)> {
        self.points
            .take()
            .into_iter()
            .map(|[id, offset, value, call]| {
                (
                    id as u32,
                    offset as i32,
                    f64::from_bits(value),
                    call as usize,
                )
            })
            .collect()
    }

    /// The events sent since the last call, as `(type, offset, note id,
    /// value)`.
    fn events(&self) -> Vec<(u16, i32, i32, f64)> {
        self.events
            .take()
            .into_iter()
            .map(|[kind, offset, id, value]| {
                (kind as u16, offset as i32, id as i32, f64::from_bits(value))
            })
            .collect()
    }
}

fn read_bytes(stream: *mut IBStream) -> Vec<u8> {
    let Some(stream) = (unsafe { ComRef::from_raw(stream) }) else {
        return Vec::new();
    };
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 16];
        let mut read = 0;
        unsafe { stream.read(chunk.as_mut_ptr().cast(), 16, &mut read) };
        if read <= 0 {
            return bytes;
        }
        bytes.extend_from_slice(&chunk[..read as usize]);
    }
}

fn write_bytes(stream: *mut IBStream, mut bytes: &[u8]) -> tresult {
    let Some(stream) = (unsafe { ComRef::from_raw(stream) }) else {
        return kInvalidArgument;
    };
    while !bytes.is_empty() {
        let mut written = 0;
        let result = unsafe {
            stream.write(
                bytes.as_ptr().cast_mut().cast(),
                bytes.len() as i32,
                &mut written,
            )
        };
        if result != kResultOk || written <= 0 {
            return kResultFalse;
        }
        bytes = &bytes[written as usize..];
    }
    kResultOk
}

fn tuid(id: [u8; 16]) -> TUID {
    id.map(|byte| byte as c_char)
}

/// Hands a new object out through the interface `iid` asks for. The query
/// takes its own reference; the one passed in is released on return.
unsafe fn hand_out(unknown: ComPtr<FUnknown>, iid: *const TUID, obj: *mut *mut c_void) -> tresult {
    let pointer = unknown.as_ptr();
    unsafe { ((*(*pointer).vtbl).queryInterface)(pointer, iid, obj) }
}

// --- The component -----------------------------------------------------------

struct Component {
    script: Arc<Script>,
    host: Mutex<Option<ComPtr<FUnknown>>>,
}

impl Class for Component {
    type Interfaces = (IComponent, IAudioProcessor, IConnectionPoint);
}

impl IPluginBaseTrait for Component {
    unsafe fn initialize(&self, context: *mut FUnknown) -> tresult {
        self.script.note("component initialize");
        *self.host.lock().unwrap() = unsafe { ComRef::from_raw(context) }.map(|c| c.to_com_ptr());
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        self.script.note("component terminate");
        *self.host.lock().unwrap() = None;
        kResultOk
    }
}

impl IComponentTrait for Component {
    unsafe fn getControllerClassId(&self, class: *mut TUID) -> tresult {
        if !self.script.has_controller.load(Relaxed) {
            return kNotImplemented;
        }
        unsafe { *class = tuid(CONTROLLER.bytes()) };
        kResultOk
    }
    unsafe fn setIoMode(&self, _mode: vst::IoMode) -> tresult {
        kNotImplemented
    }
    unsafe fn getBusCount(&self, media: vst::MediaType, _direction: vst::BusDirection) -> i32 {
        // One stereo bus each way, and one event bus each way.
        i32::from(
            media == vst::MediaTypes_::kAudio as vst::MediaType
                || media == vst::MediaTypes_::kEvent as vst::MediaType,
        )
    }
    unsafe fn getBusInfo(
        &self,
        media: vst::MediaType,
        direction: vst::BusDirection,
        index: i32,
        bus: *mut vst::BusInfo,
    ) -> tresult {
        if index != 0 || bus.is_null() {
            return kInvalidArgument;
        }
        let bus = unsafe { &mut *bus };
        bus.mediaType = media;
        bus.direction = direction;
        bus.channelCount = if media == vst::MediaTypes_::kAudio as vst::MediaType {
            2
        } else {
            1
        };
        bus.busType = vst::BusTypes_::kMain as vst::BusType;
        bus.flags = vst::BusInfo_::BusFlags_::kDefaultActive as u32;
        for (slot, unit) in bus.name.iter_mut().zip("Main".encode_utf16().chain([0])) {
            *slot = unit;
        }
        kResultOk
    }
    unsafe fn getRoutingInfo(
        &self,
        _input: *mut vst::RoutingInfo,
        _output: *mut vst::RoutingInfo,
    ) -> tresult {
        kNotImplemented
    }
    unsafe fn activateBus(
        &self,
        _media: vst::MediaType,
        _direction: vst::BusDirection,
        _index: i32,
        _state: TBool,
    ) -> tresult {
        kResultOk
    }
    unsafe fn setActive(&self, state: TBool) -> tresult {
        if state != 0 && self.script.refuse_activation.load(Relaxed) {
            return kResultFalse;
        }
        self.script
            .note(if state != 0 { "activate" } else { "deactivate" });
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let bytes = read_bytes(state);
        let Ok(bytes) = <[u8; 8]>::try_from(bytes.as_slice()) else {
            return kResultFalse;
        };
        self.script.gain.store(u64::from_le_bytes(bytes), Relaxed);
        kResultOk
    }
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        write_bytes(state, &self.script.gain().to_le_bytes())
    }
}

impl IAudioProcessorTrait for Component {
    unsafe fn setBusArrangements(
        &self,
        _inputs: *mut vst::SpeakerArrangement,
        _num_inputs: i32,
        _outputs: *mut vst::SpeakerArrangement,
        _num_outputs: i32,
    ) -> tresult {
        kResultOk
    }
    unsafe fn getBusArrangement(
        &self,
        _direction: vst::BusDirection,
        _index: i32,
        arrangement: *mut vst::SpeakerArrangement,
    ) -> tresult {
        unsafe { *arrangement = vst::SpeakerArr::kStereo };
        kResultOk
    }
    unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
        if size == vst::SymbolicSampleSizes_::kSample32 as i32 {
            kResultOk
        } else {
            kResultFalse
        }
    }
    unsafe fn getLatencySamples(&self) -> u32 {
        3
    }
    unsafe fn setupProcessing(&self, _setup: *mut vst::ProcessSetup) -> tresult {
        if self.script.refuse_setup.load(Relaxed) {
            kResultFalse
        } else {
            kResultOk
        }
    }
    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        if state != 0 && self.script.refuse_start.load(Relaxed) {
            kResultFalse
        } else {
            kResultOk
        }
    }
    unsafe fn process(&self, data: *mut vst::ProcessData) -> tresult {
        let script = &self.script;
        let call = script.process_calls.fetch_add(1, Relaxed);
        if script.fail_process.load(Relaxed) {
            return kResultFalse;
        }
        let data = unsafe { &mut *data };
        let frames = data.numSamples.max(0) as usize;
        script.calls.push([frames as u64, 0, 0, 0]);

        // The gain's points, in order, as a ramp of steps.
        let mut steps = [(0i32, 0f64); 64];
        let mut count = 0;
        if let Some(changes) = unsafe { ComRef::from_raw(data.inputParameterChanges) } {
            for index in 0..unsafe { changes.getParameterCount() } {
                let Some(queue) = (unsafe { ComRef::from_raw(changes.getParameterData(index)) })
                else {
                    continue;
                };
                let id = unsafe { queue.getParameterId() };
                for point in 0..unsafe { queue.getPointCount() } {
                    let (mut offset, mut value) = (0, 0.0);
                    unsafe { queue.getPoint(point, &mut offset, &mut value) };
                    script.points.push([
                        u64::from(id),
                        offset as u64,
                        value.to_bits(),
                        call as u64,
                    ]);
                    if id == GAIN && count < steps.len() {
                        steps[count] = (offset, value);
                        count += 1;
                    }
                }
            }
        }
        if let Some(list) = unsafe { ComRef::from_raw(data.inputEvents) } {
            for index in 0..unsafe { list.getEventCount() } {
                let mut event: vst::Event = unsafe { std::mem::zeroed() };
                unsafe { list.getEvent(index, &mut event) };
                let (id, value) = unsafe {
                    match u32::from(event.r#type) {
                        kind if kind == vst::Event_::EventTypes_::kNoteOnEvent as u32 => (
                            event.__field0.noteOn.noteId,
                            f64::from(event.__field0.noteOn.pitch),
                        ),
                        kind if kind == vst::Event_::EventTypes_::kNoteOffEvent as u32 => (
                            event.__field0.noteOff.noteId,
                            f64::from(event.__field0.noteOff.pitch),
                        ),
                        kind if kind == vst::Event_::EventTypes_::kPolyPressureEvent as u32 => (
                            event.__field0.polyPressure.noteId,
                            f64::from(event.__field0.polyPressure.pressure),
                        ),
                        kind if kind
                            == vst::Event_::EventTypes_::kNoteExpressionValueEvent as u32 =>
                        {
                            (
                                event.__field0.noteExpressionValue.noteId,
                                event.__field0.noteExpressionValue.value,
                            )
                        }
                        _ => (0, 0.0),
                    }
                };
                script.events.push([
                    u64::from(event.r#type),
                    event.sampleOffset as u64,
                    id as u64,
                    value.to_bits(),
                ]);
            }
        }

        // Input to output, times the gain, which moves on each point's
        // frame.
        let mut gain = script.gain();
        let mut next = 0;
        if data.numInputs >= 1 && data.numOutputs >= 1 {
            let (input, output) = unsafe { (&*data.inputs, &mut *data.outputs) };
            for frame in 0..frames {
                while next < count && steps[next].0 as usize <= frame {
                    gain = steps[next].1;
                    next += 1;
                }
                for channel in 0..2 {
                    unsafe {
                        let from = *(*input.__field0.channelBuffers32.add(channel)).add(frame);
                        *(*output.__field0.channelBuffers32.add(channel)).add(frame) =
                            from * (2.0 * gain) as f32;
                    }
                }
            }
        }
        while next < count {
            gain = steps[next].1;
            next += 1;
        }
        script.gain.store(gain.to_bits(), Relaxed);

        // What it has to say back.
        if script.report.load(Relaxed) && frames > 0 {
            if let Some(changes) = unsafe { ComRef::from_raw(data.outputParameterChanges) } {
                let mut index = 0;
                let queue = unsafe { changes.addParameterData(&METER, &mut index) };
                if let Some(queue) = unsafe { ComRef::from_raw(queue) } {
                    unsafe {
                        queue.addPoint(0, 0.125, &mut index);
                        queue.addPoint(frames as i32 - 1, 0.25, &mut index);
                        // Beyond the block: kept on its last frame.
                        queue.addPoint(10_000, 0.375, &mut index);
                    }
                }
                // A parameter it does not have, and a value that is none.
                let queue = unsafe { changes.addParameterData(&4_321, &mut index) };
                if let Some(queue) = unsafe { ComRef::from_raw(queue) } {
                    unsafe { queue.addPoint(1, 0.5, &mut index) };
                }
                let queue = unsafe { changes.addParameterData(&GAIN, &mut index) };
                if let Some(queue) = unsafe { ComRef::from_raw(queue) } {
                    unsafe { queue.addPoint(2, f64::NAN, &mut index) };
                }
            }
            if let Some(list) = unsafe { ComRef::from_raw(data.outputEvents) } {
                let mut event: vst::Event = unsafe { std::mem::zeroed() };
                event.sampleOffset = 5;
                event.r#type = vst::Event_::EventTypes_::kNoteOnEvent as u16;
                event.__field0.noteOn = vst::NoteOnEvent {
                    channel: 2,
                    pitch: 64,
                    tuning: 0.0,
                    velocity: 0.5,
                    length: 0,
                    noteId: 77,
                };
                unsafe { list.addEvent(&mut event) };
            }
        }
        kResultOk
    }
    unsafe fn getTailSamples(&self) -> u32 {
        480
    }
}

impl IConnectionPointTrait for Component {
    unsafe fn connect(&self, other: *mut IConnectionPoint) -> tresult {
        self.script.note("component connect");
        // Say hello through a message the host makes.
        let host = self.host.lock().unwrap().clone();
        let Some(host) = host.and_then(|host| host.cast::<IHostApplication>()) else {
            return kResultOk;
        };
        let mut class = tuid(IMessage::IID);
        let mut interface = tuid(IMessage::IID);
        let mut object = ptr::null_mut();
        if unsafe { host.createInstance(&mut class, &mut interface, &mut object) } != kResultOk {
            return kResultOk;
        }
        let message = unsafe { ComPtr::<IMessage>::from_raw(object.cast()) }.unwrap();
        unsafe {
            message.setMessageID(c"hello".as_ptr());
            if let Some(attributes) = ComRef::<IAttributeList>::from_raw(message.getAttributes()) {
                attributes.setInt(c"answer".as_ptr(), 42);
            }
            if let Some(other) = ComRef::from_raw(other) {
                other.notify(message.as_ptr());
            }
        }
        kResultOk
    }
    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        self.script.note("component disconnect");
        kResultOk
    }
    unsafe fn notify(&self, _message: *mut IMessage) -> tresult {
        kResultOk
    }
}

// --- The controller ----------------------------------------------------------

struct Controller {
    script: Arc<Script>,
    values: Mutex<[f64; 3]>,
}

impl Controller {
    fn slot(id: u32) -> Option<usize> {
        [GAIN, METER, BRIGHT].iter().position(|&known| known == id)
    }
}

impl Class for Controller {
    type Interfaces = (IEditController, IConnectionPoint, IMidiMapping);
}

impl IPluginBaseTrait for Controller {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        self.script.note("controller initialize");
        kResultOk
    }
    unsafe fn terminate(&self) -> tresult {
        self.script.note("controller terminate");
        kResultOk
    }
}

impl IEditControllerTrait for Controller {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        let bytes = read_bytes(state);
        if let Ok(gain) = <[u8; 8]>::try_from(bytes.as_slice()) {
            self.values.lock().unwrap()[0] = f64::from_le_bytes(gain);
        }
        self.script.component_states.lock().unwrap().push(bytes);
        kResultOk
    }
    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let bytes = read_bytes(state);
        self.script.controller_states.lock().unwrap().push(bytes);
        kResultOk
    }
    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        write_bytes(state, b"CTRL")
    }
    unsafe fn getParameterCount(&self) -> i32 {
        self.script.params.load(Relaxed)
    }
    unsafe fn getParameterInfo(&self, index: i32, info: *mut vst::ParameterInfo) -> tresult {
        self.script.param_info_calls.fetch_add(1, Relaxed);
        let (Some(&id), Some(info)) = ([GAIN, METER, BRIGHT].get(index as usize), unsafe {
            info.as_mut()
        }) else {
            return kResultFalse;
        };
        use vst::ParameterInfo_::ParameterFlags_ as flags;
        let (title, bits) = match id {
            GAIN => ("Gain", flags::kCanAutomate as i32),
            METER => ("Meter", flags::kIsReadOnly as i32),
            _ => ("Bright", flags::kIsHidden as i32),
        };
        info.id = id;
        for (slot, unit) in info.title.iter_mut().zip(title.encode_utf16().chain([0])) {
            *slot = unit;
        }
        info.stepCount = 0;
        info.defaultNormalizedValue = if id == GAIN { 0.5 } else { f64::NAN };
        info.unitId = 0;
        info.flags = bits;
        kResultOk
    }
    unsafe fn getParamStringByValue(
        &self,
        _id: u32,
        value: f64,
        string: *mut vst::String128,
    ) -> tresult {
        let text = format!("{:.2}", 2.0 * value);
        let out = unsafe { &mut *string };
        for (slot, unit) in out.iter_mut().zip(text.encode_utf16().chain([0])) {
            *slot = unit;
        }
        kResultOk
    }
    unsafe fn getParamValueByString(
        &self,
        _id: u32,
        string: *mut vst::TChar,
        value: *mut f64,
    ) -> tresult {
        let mut units = Vec::new();
        let mut at = 0;
        while unsafe { *string.add(at) } != 0 {
            units.push(unsafe { *string.add(at) });
            at += 1;
        }
        match String::from_utf16_lossy(&units).trim().parse::<f64>() {
            Ok(parsed) => {
                unsafe { *value = parsed / 2.0 };
                kResultOk
            }
            Err(_) => kResultFalse,
        }
    }
    unsafe fn normalizedParamToPlain(&self, _id: u32, value: f64) -> f64 {
        if self.script.nan_values.load(Relaxed) {
            f64::NAN
        } else {
            2.0 * value
        }
    }
    unsafe fn plainParamToNormalized(&self, _id: u32, plain: f64) -> f64 {
        plain / 2.0
    }
    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        if self.script.nan_values.load(Relaxed) {
            return f64::NAN;
        }
        Self::slot(id).map_or(0.0, |slot| self.values.lock().unwrap()[slot])
    }
    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        self.script.told.lock().unwrap().push((id, value));
        if let Some(slot) = Self::slot(id) {
            self.values.lock().unwrap()[slot] = value;
        }
        kResultOk
    }
    unsafe fn setComponentHandler(&self, handler: *mut IComponentHandler) -> tresult {
        self.script.note(if handler.is_null() {
            "controller handler taken"
        } else {
            "controller handler given"
        });
        *self.script.handler.lock().unwrap() =
            unsafe { ComRef::from_raw(handler) }.map(|handler| handler.to_com_ptr());
        kResultOk
    }
    unsafe fn createView(&self, name: FIDString) -> *mut IPlugView {
        let editor = !name.is_null() && unsafe { CStr::from_ptr(name) } == c"editor";
        if !editor || !self.script.has_editor.load(Relaxed) {
            return ptr::null_mut();
        }
        self.script.note("view created");
        let view = ComWrapper::new(View {
            script: Arc::clone(&self.script),
            frame: Mutex::new(None),
            this: Mutex::new(0),
            size: Mutex::new((400, 300)),
        });
        let pointer = view.to_com_ptr::<IPlugView>().unwrap();
        *view.this.lock().unwrap() = pointer.as_ptr().addr();
        pointer.into_raw()
    }
}

// --- The editor --------------------------------------------------------------

/// The controller's editor. It lives in a "window" whose handle it never
/// uses, so it runs on every system, and stands for a Win32 one: it says it
/// supports `HWND` only. It is 400 by 300 to start with and, when it may be
/// resized, takes any size from 200 by 100 to 1,000 by 800.
struct View {
    script: Arc<Script>,
    frame: Mutex<Option<ComPtr<IPlugFrame>>>,
    /// The view's own `IPlugView` pointer, for `resizeView`.
    this: Mutex<usize>,
    size: Mutex<(i32, i32)>,
}

impl Drop for View {
    fn drop(&mut self) {
        self.script.note("view released");
    }
}

impl Class for View {
    type Interfaces = (IPlugView, IPlugViewContentScaleSupport);
}

impl IPlugViewTrait for View {
    unsafe fn isPlatformTypeSupported(&self, kind: FIDString) -> tresult {
        let hwnd = unsafe { CStr::from_ptr(kind) } == c"HWND";
        if hwnd && !self.script.refuses("platform") {
            kResultOk
        } else {
            kResultFalse
        }
    }
    unsafe fn attached(&self, parent: *mut c_void, _kind: FIDString) -> tresult {
        self.script
            .note(&format!("view attached to {:#x}", parent.addr()));
        if self.script.refuses("attached") {
            return kResultFalse;
        }
        let wanted = *self.script.resize_when_attached.lock().unwrap();
        if let Some((width, height)) = wanted {
            let frame = self.frame.lock().unwrap().clone().expect("a frame");
            let this: *mut IPlugView = ptr::with_exposed_provenance_mut(*self.this.lock().unwrap());
            let mut rect = ViewRect {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            let answer = unsafe { frame.resizeView(this, &mut rect) };
            *self.script.resize_answer.lock().unwrap() = Some(answer);
        }
        kResultOk
    }
    unsafe fn removed(&self) -> tresult {
        self.script.note("view removed");
        kResultOk
    }
    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyDown(&self, _key: u16, _code: i16, _modifiers: i16) -> tresult {
        kResultFalse
    }
    unsafe fn onKeyUp(&self, _key: u16, _code: i16, _modifiers: i16) -> tresult {
        kResultFalse
    }
    unsafe fn getSize(&self, rect: *mut ViewRect) -> tresult {
        if self.script.refuses("getSize") || rect.is_null() {
            return kResultFalse;
        }
        let (width, height) = *self.size.lock().unwrap();
        unsafe {
            rect.write(ViewRect {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            })
        };
        kResultOk
    }
    unsafe fn onSize(&self, rect: *mut ViewRect) -> tresult {
        let rect = unsafe { *rect };
        let size = (rect.right - rect.left, rect.bottom - rect.top);
        self.script
            .note(&format!("view onSize {}x{}", size.0, size.1));
        if self.script.refuses("onSize") {
            return kResultFalse;
        }
        *self.size.lock().unwrap() = size;
        kResultOk
    }
    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }
    unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
        let frame = unsafe { ComRef::from_raw(frame) }.map(|frame| frame.to_com_ptr());
        self.script.note(if frame.is_some() {
            "view frame given"
        } else {
            "view frame taken"
        });
        if let Some(frame) = &frame {
            *self.script.frame.lock().unwrap() = Some(frame.clone());
        }
        *self.frame.lock().unwrap() = frame;
        if self.script.refuses("setFrame") {
            kResultFalse
        } else {
            kResultOk
        }
    }
    unsafe fn canResize(&self) -> tresult {
        if self.script.resizable.load(Relaxed) {
            kResultOk
        } else {
            kResultFalse
        }
    }
    unsafe fn checkSizeConstraint(&self, rect: *mut ViewRect) -> tresult {
        let rect = unsafe { &mut *rect };
        let width = (rect.right - rect.left).clamp(200, 1_000);
        let height = (rect.bottom - rect.top).clamp(100, 800);
        self.script.note("view checkSizeConstraint");
        rect.right = rect.left + width;
        rect.bottom = rect.top + height;
        kResultOk
    }
}

impl IPlugViewContentScaleSupportTrait for View {
    unsafe fn setContentScaleFactor(&self, factor: f32) -> tresult {
        self.script.note(&format!("view scale {factor}"));
        if self.script.refuses("scale") {
            kResultFalse
        } else {
            kResultOk
        }
    }
}

impl IConnectionPointTrait for Controller {
    unsafe fn connect(&self, _other: *mut IConnectionPoint) -> tresult {
        self.script.note("controller connect");
        kResultOk
    }
    unsafe fn disconnect(&self, _other: *mut IConnectionPoint) -> tresult {
        self.script.note("controller disconnect");
        kResultOk
    }
    unsafe fn notify(&self, message: *mut IMessage) -> tresult {
        let Some(message) = (unsafe { ComRef::from_raw(message) }) else {
            return kInvalidArgument;
        };
        let id = unsafe { CStr::from_ptr(message.getMessageID()) }
            .to_string_lossy()
            .into_owned();
        let mut answer = 0;
        if let Some(attributes) =
            unsafe { ComRef::<IAttributeList>::from_raw(message.getAttributes()) }
        {
            unsafe { attributes.getInt(c"answer".as_ptr(), &mut answer) };
        }
        self.script.messages.lock().unwrap().push((id, answer));
        kResultOk
    }
}

impl IMidiMappingTrait for Controller {
    unsafe fn getMidiControllerAssignment(
        &self,
        bus: i32,
        channel: i16,
        controller: vst::CtrlNumber,
        id: *mut vst::ParamID,
    ) -> tresult {
        if bus == 0 && channel == 0 && controller == 74 {
            unsafe { *id = BRIGHT };
            kResultOk
        } else {
            kResultFalse
        }
    }
}

// --- The factory -------------------------------------------------------------

struct Factory {
    script: Arc<Script>,
}

impl Drop for Factory {
    fn drop(&mut self) {
        // The module releases its factory just before it calls the
        // library's exit function and unloads it.
        self.script.note("factory released");
    }
}

impl Class for Factory {
    type Interfaces = (IPluginFactory,);
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(
        &self,
        info: *mut manguebyte_host::vst3::vst3::Steinberg::PFactoryInfo,
    ) -> tresult {
        let info = unsafe { &mut *info };
        // No terminator anywhere: the whole field is the text.
        info.vendor = [b'v' as c_char; 64];
        info.flags = 0;
        kResultOk
    }
    unsafe fn countClasses(&self) -> i32 {
        self.script.classes.load(Relaxed)
    }
    unsafe fn getClassInfo(
        &self,
        index: i32,
        info: *mut manguebyte_host::vst3::vst3::Steinberg::PClassInfo,
    ) -> tresult {
        self.script.class_info_calls.fetch_add(1, Relaxed);
        let info = unsafe { &mut *info };
        let (id, category, name): (ClassId, &[u8], &[u8]) = match index {
            0 => (COMPONENT, b"Audio Module Class\0", b"Fake\0"),
            1 => (
                CONTROLLER,
                b"Component Controller Class\0",
                b"Fake Controller\0",
            ),
            // Every further class repeats the first, or has no id.
            even if even % 2 == 0 => (COMPONENT, b"Audio Module Class\0", b"Again\0"),
            _ => (
                ClassId::from_words([0; 4]),
                b"Audio Module Class\0",
                b"Nobody\0",
            ),
        };
        info.cid = tuid(id.bytes());
        info.cardinality = 0x7FFF_FFFF;
        for (slot, &byte) in info.category.iter_mut().zip(category) {
            *slot = byte as c_char;
        }
        for (slot, &byte) in info.name.iter_mut().zip(name) {
            *slot = byte as c_char;
        }
        kResultOk
    }
    unsafe fn createInstance(
        &self,
        class: FIDString,
        interface: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        let wanted = ClassId::from_bytes(unsafe { *class.cast::<[u8; 16]>() });
        let iid = interface.cast::<TUID>();
        unsafe { *obj = ptr::null_mut() };
        if wanted == COMPONENT {
            let component = ComWrapper::new(Component {
                script: Arc::clone(&self.script),
                host: Mutex::new(None),
            });
            unsafe { hand_out(component.to_com_ptr().unwrap(), iid, obj) }
        } else if wanted == CONTROLLER {
            let controller = ComWrapper::new(Controller {
                script: Arc::clone(&self.script),
                values: Mutex::new([0.5, 0.0, 0.0]),
            });
            unsafe { hand_out(controller.to_com_ptr().unwrap(), iid, obj) }
        } else {
            kInvalidArgument
        }
    }
}

/// A module whose factory is the one above, following `script`.
fn module(script: &Arc<Script>) -> Vst3Module {
    let factory = ComWrapper::new(Factory {
        script: Arc::clone(script),
    })
    .to_com_ptr::<IPluginFactory>()
    .unwrap()
    .into_raw();
    // SAFETY: the factory is the one in this file, which keeps to the VST 3
    // interfaces, as far as its script lets it; its reference is handed
    // over.
    unsafe { Vst3Module::from_factory(factory.cast(), "fake") }.unwrap()
}

fn fresh_plugin(script: &Arc<Script>) -> Vst3Plugin {
    module(script).create(COMPONENT).unwrap()
}

fn stereo(value: f32, frames: usize) -> AudioBuffer {
    let mut buffer = AudioBuffer::new(2, frames);
    for channel in buffer.iter_channels_mut() {
        channel.fill(value);
    }
    buffer
}

const RATE: f64 = 48_000.0;

// --- The tests ---------------------------------------------------------------

#[test]
fn two_objects_are_connected_kept_in_step_and_taken_apart_in_order() {
    let script = Script::new();
    let module = module(&script);
    // Both classes are listed; only the component can be made a plugin.
    assert_eq!(module.classes().len(), 2);
    assert_eq!(module.classes()[1].category, "Component Controller Class");
    assert_eq!(
        module.create(CONTROLLER).unwrap_err(),
        Vst3Error::NotAnAudioModule(CONTROLLER)
    );
    // A field with no terminator is read to its end.
    assert_eq!(module.factory_info().vendor, "v".repeat(64));

    let mut plugin = module.create(COMPONENT).unwrap();
    assert!(plugin.has_controller());
    assert_eq!(
        script.log(),
        [
            "component initialize",
            "controller initialize",
            "component connect",
            "controller connect",
            "controller handler given",
        ]
    );
    // The component spoke to the controller through a message the host
    // made, and the controller was given the component's state.
    assert_eq!(*script.messages.lock().unwrap(), [("hello".to_owned(), 42)]);
    assert_eq!(
        *script.component_states.lock().unwrap(),
        [0.5f64.to_le_bytes().to_vec()]
    );

    // Parameters come from the controller. A default that is not a number
    // is held to the range.
    let params = plugin.params();
    assert_eq!(params.len(), 3);
    assert_eq!(params[0].title, "Gain");
    assert!(params[1].flags.read_only && params[2].flags.hidden);
    assert_eq!(params[1].default, 0.0);
    assert_eq!(
        plugin.param_to_text(ParamId(GAIN), 0.25).as_deref(),
        Some("0.50")
    );
    assert_eq!(plugin.text_to_param(ParamId(GAIN), "1.5"), Some(0.75));
    assert_eq!(plugin.text_to_param(ParamId(GAIN), "9"), Some(1.0));
    assert_eq!(plugin.normalized_to_plain(ParamId(GAIN), 0.25), Some(0.5));
    assert_eq!(plugin.midi_mapping(0, 74), Some(ParamId(BRIGHT)));
    assert_eq!(plugin.latency(), 0, "latency is read when activated");

    // State: the component's and the controller's, each back where it came
    // from, and the controller is told the component's again.
    script.gain.store(0.25f64.to_bits(), Relaxed);
    let saved = plugin.save_state().unwrap();
    script.gain.store(0.9f64.to_bits(), Relaxed);
    plugin.load_state(&saved).unwrap();
    assert_eq!(script.gain(), 0.25);
    assert_eq!(
        script.component_states.lock().unwrap().last().unwrap(),
        &0.25f64.to_le_bytes().to_vec()
    );
    assert_eq!(
        *script.controller_states.lock().unwrap(),
        [b"CTRL".to_vec()]
    );
    assert_eq!(plugin.param_value(ParamId(GAIN)), Some(0.25));

    // Dropped: deactivated, disconnected, terminated, controller first, on
    // this thread.
    let mut processor = plugin.activate(RATE, 64).unwrap();
    assert_eq!(plugin.latency(), 3);
    processor.process(&mut stereo(1.0, 64));
    drop(processor);
    drop(plugin);
    let log = script.log();
    let tail: Vec<&str> = log.iter().rev().take(6).rev().map(String::as_str).collect();
    assert_eq!(
        tail,
        [
            "deactivate",
            "component disconnect",
            "controller disconnect",
            "controller handler taken",
            "controller terminate",
            "component terminate",
        ]
    );
    let here = std::thread::current().id();
    assert_eq!(script.thread_of("component terminate"), Some(here));
    assert_eq!(script.thread_of("controller terminate"), Some(here));
}

#[test]
fn automation_and_notes_arrive_as_vst3_has_them() {
    let script = Script::new();
    let mut plugin = fresh_plugin(&script);
    let mut processor = plugin.activate(RATE, 64).unwrap();
    let note = NoteRef::with_id(1, 60, 33);
    let mut events = EventBuffer::with_capacity(32);
    let gain = ParamId(GAIN);
    for (offset, kind) in [
        (
            3,
            EventKind::ParamValue {
                param: gain,
                value: 0.25,
            },
        ),
        (
            5,
            EventKind::NoteOn {
                note,
                velocity: 0.5,
            },
        ),
        (
            10,
            EventKind::ParamValue {
                param: gain,
                value: 0.5,
            },
        ),
        (
            10,
            EventKind::ParamValue {
                param: gain,
                value: 0.75,
            },
        ),
        (
            20,
            EventKind::NoteExpression {
                note,
                expression: Expression::Tuning,
                value: 60.0,
            },
        ),
        (
            30,
            EventKind::NoteExpression {
                note,
                expression: Expression::Pressure,
                value: 0.5,
            },
        ),
        (
            70,
            EventKind::Midi(manguebyte_midi::MidiMessage::ControlChange {
                channel: 0,
                controller: 74,
                value: 127,
            }),
        ),
        (
            130,
            EventKind::ParamValue {
                param: gain,
                value: 1.0,
            },
        ),
        // Unmapped, unknown and impossible: none of these can be sent.
        (
            140,
            EventKind::Midi(manguebyte_midi::MidiMessage::PitchBend {
                channel: 0,
                value: 100,
            }),
        ),
        (
            141,
            EventKind::ParamValue {
                param: ParamId(999),
                value: 0.5,
            },
        ),
        (
            142,
            EventKind::NoteExpression {
                note: NoteRef::new(0, 61),
                expression: Expression::Volume,
                value: 1.0,
            },
        ),
        // On the frame after the block: kept for the next one.
        (
            200,
            EventKind::NoteOff {
                note,
                velocity: 0.0,
            },
        ),
    ] {
        events.push(Event { offset, kind }).unwrap();
    }
    let transport = Transport::playing_at(120.0);
    let mut block = stereo(1.0, 200);
    let heap = common::heap_calls_during(|| {
        processor.process_with(&mut block, &ProcessContext::new(&events, &transport));
    });
    assert_eq!(heap, 0);
    assert_eq!(processor.status(), ProcessStatus::Processed);

    // A block of 200 for a plugin activated for 64: four calls, every
    // point on its frame within its piece, in its parameter's queue.
    let calls: Vec<u64> = script.calls.take().iter().map(|call| call[0]).collect();
    assert_eq!(calls, [64, 64, 64, 8]);
    let first = script.process_calls.load(Relaxed) - 4;
    let points = script.points();
    assert_eq!(
        points,
        [
            (GAIN, 3, 0.25, first),
            (GAIN, 10, 0.5, first),
            (GAIN, 10, 0.75, first),
            (BRIGHT, 70 - 64, 1.0, first + 1),
            (GAIN, 130 - 128, 1.0, first + 2),
        ]
    );
    // The gain is heard from its frame: 2 x 0.5 before frame 3, then each
    // point's.
    let out = block.channel(0);
    assert_eq!(&out[..3], &[1.0; 3]);
    assert_eq!(&out[3..10], &[0.5; 7]);
    assert_eq!(&out[10..130], &[1.5; 120]);
    assert_eq!(&out[130..], &[2.0; 70]);

    // Notes, tuning as VST 3 scales it, and pressure, with the note's id.
    use vst::Event_::EventTypes_ as types;
    let events = script.events();
    assert_eq!(
        events,
        [
            (types::kNoteOnEvent as u16, 5, 33, 60.0),
            (types::kNoteExpressionValueEvent as u16, 20, 33, 0.75),
            (types::kPolyPressureEvent as u16, 30, 33, 0.5),
        ]
    );
    assert_eq!(plugin.undeliverable_events(), 3);
    assert_eq!(plugin.lost_events(), 0);

    // The note-off arrives on the first frame of the next block.
    processor.process(&mut stereo(1.0, 16));
    assert_eq!(
        script.events(),
        [(types::kNoteOffEvent as u16, 0, 33, 60.0)]
    );

    // And the controller is told what the processor was sent.
    plugin.pump();
    let told = script.told.lock().unwrap().clone();
    assert!(told.contains(&(GAIN, 1.0)) && told.contains(&(BRIGHT, 1.0)));
    assert_eq!(plugin.param_value(gain), Some(1.0));
}

#[test]
fn edits_from_the_controller_reach_the_processor_and_the_caller() {
    let script = Script::new();
    let mut plugin = fresh_plugin(&script);
    let mut processor = plugin.activate(RATE, 64).unwrap();
    // The latency read at activation is reported first.
    assert_eq!(plugin.pump(), [HostEvent::LatencyChanged(3)]);
    let handler = script.handler();
    unsafe {
        assert_eq!(handler.beginEdit(GAIN), kResultOk);
        assert_eq!(handler.performEdit(GAIN, 0.75), kResultOk);
        assert_eq!(handler.performEdit(GAIN, 7.0), kResultOk);
        assert_eq!(handler.endEdit(GAIN), kResultOk);
        // About a parameter the controller never listed.
        assert_eq!(handler.performEdit(1_234, 0.5), kResultOk);
        assert_eq!(handler.performEdit(GAIN, f64::NAN), kInvalidArgument);
    }
    let events = plugin.pump();
    assert_eq!(
        &events[..4],
        &[
            HostEvent::GestureBegin {
                param: ParamId(GAIN)
            },
            HostEvent::ParamValue {
                param: ParamId(GAIN),
                value: 0.75
            },
            HostEvent::ParamValue {
                param: ParamId(GAIN),
                value: 1.0
            },
            HostEvent::GestureEnd {
                param: ParamId(GAIN)
            },
        ]
    );
    assert!(matches!(&events[4], HostEvent::Remark(text) if text.contains("1234")));
    assert_eq!(events.len(), 5);

    // The processor hears the edits on the first frame of the next block.
    processor.process(&mut stereo(1.0, 32));
    let points: Vec<(u32, i32, f64)> = script
        .points()
        .into_iter()
        .map(|(id, offset, value, _)| (id, offset, value))
        .collect();
    assert_eq!(points, [(GAIN, 0, 0.75), (GAIN, 0, 1.0)]);

    // Restart requests: one that changes the list is acted on here, one
    // that needs a new activation is passed on, and so are the others.
    use vst::RestartFlags_ as flags;
    let second = handler.cast::<IComponentHandler2>().unwrap();
    unsafe {
        handler.restartComponent(flags::kParamTitlesChanged as i32);
        handler.restartComponent((flags::kLatencyChanged | flags::kParamValuesChanged) as i32);
        second.setDirty(1);
        second.requestOpenEditor(c"editor".as_ptr());
    }
    let events = plugin.pump();
    assert_eq!(
        events,
        [
            HostEvent::ParamsRescanned {
                values: true,
                info: true
            },
            HostEvent::RestartRequested {
                flags: flags::kLatencyChanged as i32
            },
            HostEvent::StateDirty,
            HostEvent::EditorRequested,
        ]
    );

    // An edit from the wrong thread is still taken, and remarked on.
    let elsewhere = handler.clone();
    std::thread::spawn(move || unsafe {
        elsewhere.performEdit(GAIN, 0.25);
    })
    .join()
    .unwrap();
    let events = plugin.pump();
    assert!(events.contains(&HostEvent::ParamValue {
        param: ParamId(GAIN),
        value: 0.25
    }));
    assert!(
        events.iter().any(
            |event| matches!(event, HostEvent::Remark(text) if text.contains("another thread"))
        ),
        "{events:?}"
    );

    // With no audio half, an edit waits for the next activation.
    drop(processor);
    plugin.deactivate().unwrap();
    unsafe { handler.performEdit(GAIN, 0.125) };
    plugin.pump();
    let mut processor = plugin.activate(RATE, 64).unwrap();
    processor.process(&mut stereo(1.0, 8));
    let points = script.points();
    assert_eq!(
        points.last().map(|point| (point.0, point.1, point.2)),
        Some((GAIN, 0, 0.125))
    );
}

/// An edit the controller made before a state was loaded, and that the
/// host had not yet passed on, is still reported, but the processor never
/// hears it: the state came after it. Whether audio runs or not.
#[test]
fn an_edit_made_before_a_state_is_loaded_does_not_reach_the_processor_after_it() {
    let script = Script::new();
    let mut plugin = fresh_plugin(&script);
    let gain = ParamId(GAIN);
    script.gain.store(0.25f64.to_bits(), Relaxed);
    let saved = plugin.save_state().unwrap();
    script.gain.store(0.5f64.to_bits(), Relaxed);

    // Audio runs.
    let mut processor = plugin.activate(RATE, 64).unwrap();
    processor.process(&mut stereo(1.0, 16));
    plugin.pump();
    script.points();
    unsafe { script.handler().performEdit(GAIN, 0.875) };
    plugin.load_state(&saved).unwrap();
    assert_eq!(script.gain(), 0.25);
    assert!(
        plugin.pump().contains(&HostEvent::ParamValue {
            param: gain,
            value: 0.875
        }),
        "the edit was not reported"
    );
    processor.process(&mut stereo(1.0, 16));
    let heard: Vec<_> = script
        .points()
        .into_iter()
        .filter(|point| point.0 == GAIN)
        .collect();
    assert!(
        heard.is_empty(),
        "the processor heard {heard:?} after the state"
    );
    assert_eq!(script.gain(), 0.25);
    assert_eq!(plugin.param_value(gain), Some(0.25));

    // No audio half: the edit would have waited for the next activation.
    drop(processor);
    plugin.deactivate().unwrap();
    script.gain.store(0.5f64.to_bits(), Relaxed);
    unsafe { script.handler().performEdit(GAIN, 0.875) };
    plugin.load_state(&saved).unwrap();
    plugin.pump();
    let mut processor = plugin.activate(RATE, 64).unwrap();
    processor.process(&mut stereo(1.0, 16));
    let heard: Vec<_> = script
        .points()
        .into_iter()
        .filter(|point| point.0 == GAIN)
        .collect();
    assert!(
        heard.is_empty(),
        "the processor heard {heard:?} after the state"
    );
    assert_eq!(script.gain(), 0.25);
    assert_eq!(plugin.param_value(gain), Some(0.25));

    // An edit made after the load is passed on as usual.
    unsafe { script.handler().performEdit(GAIN, 0.625) };
    plugin.pump();
    processor.process(&mut stereo(1.0, 16));
    assert_eq!(script.gain(), 0.625);
}

#[test]
fn what_the_processor_reports_comes_back() {
    let script = Script::new();
    let mut plugin = fresh_plugin(&script);
    let mut processor = plugin.activate(RATE, 64).unwrap();
    // The latency read at activation is reported first.
    assert_eq!(plugin.pump(), [HostEvent::LatencyChanged(3)]);
    script.report.store(true, Relaxed);
    let mut block = stereo(1.0, 100);
    let heap = common::heap_calls_during(|| processor.process(&mut block));
    assert_eq!(heap, 0);

    // Two calls, of 64 and 36 frames, each reporting the meter three times
    // and a note: on the frames they fall on, in the caller's block, with
    // the point beyond the call on its last frame. The unknown parameter
    // and the value that is not one are not passed on.
    let meter = |offset, value| Event {
        offset,
        kind: EventKind::ParamValue {
            param: ParamId(METER),
            value,
        },
    };
    let note = |offset| Event {
        offset,
        kind: EventKind::NoteOn {
            note: NoteRef::with_id(2, 64, 77),
            velocity: 0.5,
        },
    };
    assert_eq!(
        processor.output_events(),
        [
            note(5),
            meter(0, 0.125),
            meter(63, 0.25),
            meter(63, 0.375),
            note(64 + 5),
            meter(64, 0.125),
            meter(99, 0.25),
            meter(99, 0.375),
        ]
    );

    // The main thread hears every one, and tells the controller.
    let reported: Vec<HostEvent> = plugin.pump();
    assert_eq!(reported.len(), 6);
    assert!(reported.iter().all(|event| matches!(
        event,
        HostEvent::ParamValue { param, .. } if *param == ParamId(METER)
    )));
    assert_eq!(script.told.lock().unwrap().last(), Some(&(METER, 0.375)));
    assert_eq!(plugin.lost_events(), 0);
}

#[test]
fn a_plugin_that_fails_leaves_silence_and_says_so() {
    let script = Script::new();
    let mut plugin = fresh_plugin(&script);
    let mut processor = plugin.activate(RATE, 64).unwrap();
    // The latency read at activation is reported first.
    assert_eq!(plugin.pump(), [HostEvent::LatencyChanged(3)]);

    script.fail_process.store(true, Relaxed);
    let mut block = stereo(1.0, 64);
    processor.process(&mut block);
    assert_eq!(block.peak(), 0.0);
    assert_eq!(
        processor.status(),
        ProcessStatus::Failed(ProcessFailure::Error)
    );
    assert_eq!(plugin.failed_blocks(), 1);
    assert_eq!(
        plugin.pump(),
        [HostEvent::ProcessFailed(ProcessFailure::Error)]
    );
    script.fail_process.store(false, Relaxed);
    processor.process(&mut block);
    assert_eq!(processor.status(), ProcessStatus::Processed);
    drop(processor);
    plugin.deactivate().unwrap();

    // A plugin that will not start is asked again on every block, and the
    // events of the blocks it missed reach it once it does.
    script.refuse_start.store(true, Relaxed);
    let mut processor = plugin.activate(RATE, 64).unwrap();
    let mut events = EventBuffer::with_capacity(2);
    let note = NoteRef::with_id(0, 50, 5);
    events
        .push(Event {
            offset: 7,
            kind: EventKind::NoteOn {
                note,
                velocity: 1.0,
            },
        })
        .unwrap();
    let mut block = stereo(1.0, 64);
    processor.process_with(
        &mut block,
        &ProcessContext::new(&events, &Transport::STOPPED),
    );
    assert_eq!(block.peak(), 0.0);
    assert_eq!(
        processor.status(),
        ProcessStatus::Failed(ProcessFailure::StartRefused)
    );
    assert!(script.events().is_empty());
    script.refuse_start.store(false, Relaxed);
    processor.process(&mut stereo(1.0, 64));
    assert_eq!(processor.status(), ProcessStatus::Processed);
    let delivered = script.events();
    assert_eq!(delivered.len(), 1);
    assert_eq!((delivered[0].1, delivered[0].2), (0, 5));
    assert_eq!(plugin.lost_events(), 0);
    assert_eq!(
        plugin.pump(),
        [HostEvent::ProcessFailed(ProcessFailure::StartRefused)]
    );
    drop(processor);
    plugin.deactivate().unwrap();

    // Refusals at activation are errors, and leave the plugin inactive.
    script.refuse_setup.store(true, Relaxed);
    assert_eq!(
        plugin.activate(RATE, 64).unwrap_err(),
        Vst3Error::SetupRefused
    );
    script.refuse_setup.store(false, Relaxed);
    script.refuse_activation.store(true, Relaxed);
    assert_eq!(
        plugin.activate(RATE, 64).unwrap_err(),
        Vst3Error::ActivationFailed
    );
    assert!(!plugin.is_active() && !plugin.has_processor());
    script.refuse_activation.store(false, Relaxed);
    assert!(plugin.activate(RATE, 64).is_ok());
}

#[test]
fn limits_hold_whatever_the_plugin_claims() {
    // A factory that says it has two billion classes is asked about the
    // first 1,024; repeats and classes with no id are left out.
    let script = Script::new();
    script.classes.store(i32::MAX, Relaxed);
    let module = module(&script);
    assert_eq!(script.class_info_calls.load(Relaxed), MAX_CLASSES);
    assert_eq!(module.classes().len(), 2);

    // A controller that says it has two billion parameters is asked about
    // the first 65,536, and the host says so.
    script.params.store(i32::MAX, Relaxed);
    let mut plugin = module.create(COMPONENT).unwrap();
    assert_eq!(script.param_info_calls.load(Relaxed), MAX_PARAMS);
    assert_eq!(plugin.params().len(), 3);
    let remarks = plugin.pump();
    assert_eq!(remarks.len(), 2, "{remarks:?}");

    // Numbers that are not numbers are no answer.
    script.nan_values.store(true, Relaxed);
    assert_eq!(plugin.param_value(ParamId(GAIN)), None);
    assert_eq!(plugin.normalized_to_plain(ParamId(GAIN), 0.5), None);
    assert!(matches!(
        plugin.set_param(ParamId(GAIN), f64::NAN),
        Err(Vst3Error::InvalidArgument(_))
    ));
    script.nan_values.store(false, Relaxed);

    // A component with no controller is hosted, without parameters.
    let script = Script::new();
    script.has_controller.store(false, Relaxed);
    let mut plugin = fresh_plugin(&script);
    assert!(!plugin.has_controller() && plugin.params().is_empty());
    assert!(matches!(plugin.pump().as_slice(), [HostEvent::Remark(_)]));
    let mut processor = plugin.activate(RATE, 64).unwrap();
    let mut block = stereo(1.0, 64);
    processor.process(&mut block);
    assert_eq!(block.channel(0), [1.0; 64]);
    assert!(plugin.save_state().is_ok());
}

#[test]
fn the_plugin_is_terminated_on_its_main_thread_whichever_half_goes_first() {
    // The audio half goes first, on another thread: the main half
    // terminates the plugin when it goes, here.
    let script = Script::new();
    let mut plugin = fresh_plugin(&script);
    let processor = plugin.activate(RATE, 64).unwrap();
    std::thread::spawn(move || drop(processor)).join().unwrap();
    drop(plugin);
    let here = std::thread::current().id();
    assert_eq!(script.thread_of("component terminate"), Some(here));

    // The main half goes first and the audio half follows on this thread:
    // terminated here, by the audio half.
    let script = Script::new();
    let mut plugin = fresh_plugin(&script);
    let processor = plugin.activate(RATE, 64).unwrap();
    drop(plugin);
    assert_eq!(script.thread_of("component terminate"), None);
    drop(processor);
    assert_eq!(script.thread_of("component terminate"), Some(here));

    // The main half goes first, and the audio half follows on a thread
    // started later: no thread is left on which the plugin may be
    // terminated, so it is not.
    let script = Script::new();
    let processor = std::thread::spawn({
        let script = Arc::clone(&script);
        move || {
            let mut plugin = fresh_plugin(&script);
            let processor = plugin.activate(RATE, 64).unwrap();
            drop(plugin);
            processor
        }
    })
    .join()
    .unwrap();
    std::thread::spawn(move || drop(processor)).join().unwrap();
    assert_eq!(script.thread_of("component terminate"), None);
    assert_eq!(script.thread_of("controller terminate"), None);
}

#[test]
fn the_module_outlives_every_half_of_every_plugin_made_from_it() {
    // Two plugins, and the caller lets go of the module at once. The
    // factory is released, and with it the library's code, only when the
    // last half of the last plugin has gone, whichever that is.
    let script = Script::new();
    let module = module(&script);
    let mut first = module.create(COMPONENT).unwrap();
    let second = module.create(COMPONENT).unwrap();
    drop(module);
    let mut processor = first.activate(RATE, 64).unwrap();
    drop(first);
    drop(second);
    assert_eq!(
        script.thread_of("factory released"),
        None,
        "the module was released under a live plugin"
    );
    // The half that is left still runs the plugin's code.
    processor.process(&mut stereo(1.0, 64));
    assert_eq!(script.process_calls.load(Relaxed), 1);
    drop(processor);
    let log = script.log();
    assert_eq!(log.last().map(String::as_str), Some("factory released"));
    assert_eq!(
        log.iter()
            .filter(|what| *what == "component terminate")
            .count(),
        2
    );
}

// --- The editor --------------------------------------------------------------

/// A window of the kind the editor goes in. The editor never uses it.
fn window() -> ParentWindow {
    ParentWindow::win32(0x1234).unwrap()
}

/// Asks for a size through the frame the editor was last given, from
/// outside any call of the host's, as the editor's own window would.
fn resize_through_frame(script: &Script, width: i32, height: i32) -> tresult {
    let frame = script.frame.lock().unwrap().clone().expect("a frame");
    let mut rect = ViewRect {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    unsafe { frame.resizeView(ptr::null_mut(), &mut rect) }
}

#[test]
fn an_editor_opens_resizes_and_closes_in_the_order_vst3_asks() {
    let script = Script::new();
    let mut plugin = fresh_plugin(&script);
    assert!(plugin.has_editor());
    assert!(!plugin.is_editor_open());
    assert_eq!(plugin.editor_size(), None);
    assert!(!plugin.can_resize_editor());
    let size = EditorSize::new(10, 10);
    assert_eq!(plugin.set_editor_size(size), Err(Vst3Error::EditorNotOpen));
    assert_eq!(plugin.set_editor_scale(2.0), Err(Vst3Error::EditorNotOpen));
    plugin.close_editor();

    // The frame before the scale, the scale before the window, then the
    // size is asked for.
    assert_eq!(
        plugin.open_editor(window(), Some(1.5)),
        Ok(EditorSize::new(400, 300))
    );
    assert_eq!(
        script.view_log(),
        [
            "view created",
            "view frame given",
            "view scale 1.5",
            "view attached to 0x1234"
        ]
    );
    assert!(plugin.is_editor_open());
    assert_eq!(plugin.editor_size(), Some(EditorSize::new(400, 300)));
    assert_eq!(
        plugin.open_editor(window(), None),
        Err(Vst3Error::EditorAlreadyOpen)
    );

    // A resizable editor is asked what it can take, and given that.
    assert!(plugin.can_resize_editor());
    assert_eq!(
        plugin.set_editor_size(EditorSize::new(5_000, 50)),
        Ok(EditorSize::new(1_000, 100))
    );
    assert_eq!(plugin.editor_size(), Some(EditorSize::new(1_000, 100)));
    let log = script.view_log();
    assert_eq!(
        log[log.len() - 2..],
        ["view checkSizeConstraint", "view onSize 1000x100"]
    );

    // Scales that are not positive numbers, in `f32` too, are refused
    // before the editor sees them.
    assert_eq!(plugin.set_editor_scale(2.0), Ok(()));
    for scale in [0.0, -1.0, f64::NAN, f64::INFINITY, 1e300, 1e-300] {
        assert!(
            matches!(
                plugin.set_editor_scale(scale),
                Err(Vst3Error::InvalidArgument(_))
            ),
            "{scale}"
        );
    }
    assert_eq!(script.view_log().last().unwrap(), "view scale 2");

    // Closed: removed, the frame taken back, released.
    plugin.close_editor();
    let log = script.view_log();
    assert_eq!(
        log[log.len() - 3..],
        ["view removed", "view frame taken", "view released"]
    );
    assert!(!plugin.is_editor_open());
    assert_eq!(plugin.editor_size(), None);
    assert!(!plugin.can_resize_editor());
    plugin.close_editor();
    assert_eq!(script.view_log().len(), log.len(), "closed twice");
    // The frame the plugin kept refuses everything now.
    assert_eq!(resize_through_frame(&script, 500, 500), kResultFalse);
    assert!(
        !plugin
            .pump()
            .iter()
            .any(|event| matches!(event, HostEvent::Editor(_)))
    );

    // Opened again with no scale, and dropped with the editor open: the
    // editor goes first, then the controller and the component.
    assert_eq!(
        plugin.open_editor(window(), None),
        Ok(EditorSize::new(400, 300))
    );
    assert!(
        !script.view_log()[log.len()..]
            .iter()
            .any(|what| what.starts_with("view scale"))
    );
    drop(plugin);
    let log = script.log();
    let at = |what: &str| log.iter().rposition(|seen| seen == what).expect(what);
    assert!(at("view removed") < at("view frame taken"));
    assert!(at("view frame taken") < at("view released"));
    assert!(at("view released") < at("controller terminate"));
    assert!(at("view released") < at("component terminate"));
}

#[test]
fn an_editor_that_cannot_open_leaves_nothing_behind() {
    let script = Script::new();
    let mut plugin = fresh_plugin(&script);
    let cases: [(&str, &[&str]); 3] = [
        ("setFrame", &["view frame given", "view frame taken"]),
        (
            "attached",
            &[
                "view frame given",
                "view attached to 0x1234",
                "view frame taken",
            ],
        ),
        (
            "getSize",
            &[
                "view frame given",
                "view attached to 0x1234",
                "view removed",
                "view frame taken",
            ],
        ),
    ];
    for (call, between) in cases {
        *script.view_refuses.lock().unwrap() = Some(call);
        // The editor asks for something on the way, which has to go too.
        *script.resize_when_attached.lock().unwrap() = Some((640, 480));
        let before = script.view_log().len();
        assert_eq!(
            plugin.open_editor(window(), None),
            Err(Vst3Error::EditorRefused(call))
        );
        let mut expected = vec!["view created"];
        expected.extend_from_slice(between);
        expected.push("view released");
        assert_eq!(script.view_log()[before..], expected, "{call}");
        assert!(!plugin.is_editor_open(), "{call}");
        assert_eq!(plugin.editor_size(), None, "{call}");
        assert_eq!(resize_through_frame(&script, 500, 500), kResultFalse);
        assert!(
            !plugin
                .pump()
                .iter()
                .any(|event| matches!(event, HostEvent::Editor(_))),
            "{call}"
        );
        assert_eq!(plugin.next_editor_timer(), None);
        assert!(plugin.editor_fds().is_empty());
    }
    *script.resize_when_attached.lock().unwrap() = None;

    // A window of a kind the editor cannot live in, as it says itself, or
    // as it says of every kind but one.
    *script.view_refuses.lock().unwrap() = Some("platform");
    let before = script.view_log().len();
    assert_eq!(
        plugin.open_editor(window(), None),
        Err(Vst3Error::PlatformUnsupported(WindowApi::Win32))
    );
    *script.view_refuses.lock().unwrap() = None;
    assert_eq!(
        plugin.open_editor(ParentWindow::x11(5).unwrap(), None),
        Err(Vst3Error::PlatformUnsupported(WindowApi::X11))
    );
    assert_eq!(
        script.view_log()[before..],
        [
            "view created",
            "view released",
            "view created",
            "view released"
        ]
    );

    // A scale that is no number is refused before anything is made.
    let before = script.view_log().len();
    assert!(matches!(
        plugin.open_editor(window(), Some(f64::NAN)),
        Err(Vst3Error::InvalidArgument(_))
    ));
    assert_eq!(script.view_log().len(), before);

    // A controller with no editor, and a plugin with no controller.
    script.has_editor.store(false, Relaxed);
    assert_eq!(plugin.open_editor(window(), None), Err(Vst3Error::NoView));
    script.has_editor.store(true, Relaxed);
    let lonely = Script::new();
    lonely.has_controller.store(false, Relaxed);
    let mut alone = fresh_plugin(&lonely);
    assert!(!alone.has_editor());
    assert_eq!(
        alone.open_editor(window(), None),
        Err(Vst3Error::NoController)
    );

    // After all of that, the editor opens, and a refused scale does not
    // keep it from opening.
    *script.view_refuses.lock().unwrap() = Some("scale");
    assert_eq!(
        plugin.open_editor(window(), Some(2.0)),
        Ok(EditorSize::new(400, 300))
    );
    assert_eq!(
        plugin.set_editor_scale(1.0),
        Err(Vst3Error::EditorRefused("setContentScaleFactor"))
    );
}

#[test]
fn what_the_editor_asks_for_comes_back_from_pump_and_is_answered_with_its_size() {
    let script = Script::new();
    let mut plugin = fresh_plugin(&script);
    plugin.pump();

    // Asked from inside `attached`: granted, and passed on. The editor
    // keeps its size until the application gives it the new one.
    *script.resize_when_attached.lock().unwrap() = Some((640, 480));
    assert_eq!(
        plugin.open_editor(window(), None),
        Ok(EditorSize::new(400, 300))
    );
    assert_eq!(*script.resize_answer.lock().unwrap(), Some(kResultTrue));
    let wanted = EditorSize::new(640, 480);
    assert_eq!(
        plugin.pump(),
        [HostEvent::Editor(EditorRequest::Resize(wanted))]
    );
    assert_eq!(plugin.pump(), []);
    assert_eq!(plugin.editor_size(), Some(EditorSize::new(400, 300)));
    assert_eq!(plugin.set_editor_size(wanted), Ok(wanted));
    assert_eq!(script.view_log().last().unwrap(), "view onSize 640x480");

    // Asked from the editor's own window, twice before the application
    // looks: only the latest counts.
    assert_eq!(resize_through_frame(&script, 700, 500), kResultTrue);
    assert_eq!(resize_through_frame(&script, 720, 520), kResultTrue);
    assert_eq!(
        plugin.pump(),
        [HostEvent::Editor(EditorRequest::Resize(EditorSize::new(
            720, 520
        )))]
    );
    let mut inside_out = ViewRect {
        left: 10,
        top: 0,
        right: 0,
        bottom: 10,
    };
    let frame = script.frame.lock().unwrap().clone().unwrap();
    assert_eq!(
        unsafe { frame.resizeView(ptr::null_mut(), &mut inside_out) },
        kInvalidArgument
    );

    // From another thread, refused and remarked on.
    let elsewhere = frame.clone();
    std::thread::spawn(move || {
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 1,
            bottom: 1,
        };
        assert_eq!(
            unsafe { elsewhere.resizeView(ptr::null_mut(), &mut rect) },
            kResultFalse
        );
    })
    .join()
    .unwrap();
    let events = plugin.pump();
    assert!(
        matches!(&events[..], [HostEvent::Remark(text)] if text.contains("editor's frame")),
        "{events:?}"
    );

    // An editor of fixed size is given the size as it is, and may refuse
    // it, keeping its own.
    script.resizable.store(false, Relaxed);
    assert!(!plugin.can_resize_editor());
    let before = script.view_log().len();
    assert_eq!(
        plugin.set_editor_size(EditorSize::new(300, 200)),
        Ok(EditorSize::new(300, 200))
    );
    assert_eq!(script.view_log()[before..], ["view onSize 300x200"]);
    *script.view_refuses.lock().unwrap() = Some("onSize");
    assert_eq!(
        plugin.set_editor_size(EditorSize::new(320, 240)),
        Err(Vst3Error::EditorRefused("onSize"))
    );
    assert_eq!(plugin.editor_size(), Some(EditorSize::new(300, 200)));
    *script.view_refuses.lock().unwrap() = None;

    // The plugin asks for its editor: met while it is open, passed on
    // either way. Another view is refused.
    let second = script.handler().cast::<IComponentHandler2>().unwrap();
    unsafe {
        assert_eq!(second.requestOpenEditor(c"editor".as_ptr()), kResultTrue);
    }
    assert_eq!(plugin.pump(), [HostEvent::EditorRequested]);
    plugin.close_editor();
    unsafe {
        assert_eq!(second.requestOpenEditor(c"editor".as_ptr()), kResultFalse);
        assert_eq!(second.requestOpenEditor(c"other".as_ptr()), kResultFalse);
    }
    let events = plugin.pump();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0], HostEvent::EditorRequested);
    assert!(
        matches!(&events[1], HostEvent::Remark(text) if text.contains("other than its editor"))
    );
}
