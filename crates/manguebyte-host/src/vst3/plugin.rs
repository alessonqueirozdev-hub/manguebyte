//! The main-thread half of a hosted VST 3 plugin.

// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast that is needed on one platform is redundant on the
// other.
#![allow(clippy::unnecessary_cast)]

use std::ffi::c_void;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use manguebyte_core::ParamId;
use manguebyte_core::spsc::{self, Consumer, Producer};
use vst3::Steinberg::Vst::{
    self as vst, BusDirections_, BusTypes_, IAudioProcessor, IAudioProcessorTrait, IComponent,
    IComponentTrait, IConnectionPoint, IConnectionPointTrait, IEditController,
    IEditControllerTrait, IMidiMapping, IMidiMappingTrait, MediaTypes_, RestartFlags_,
};
use vst3::Steinberg::{
    self as sb, FIDString, IPlugView, IPlugViewContentScaleSupport,
    IPlugViewContentScaleSupportTrait, IPlugViewTrait, IPluginBaseTrait, TUID, ViewRect,
    kNotImplemented, kResultOk, kResultTrue,
};
use vst3::{ComPtr, ComRef, ComWrapper};

use super::convert::{CONTROLLERS, MidiMap, unit};
use super::frame::{EditorLoop, Frame, frame_ptr, rect_of, rect_size};
use super::module::ClassInfo;
use super::objects::{
    ComponentHandler, Edit, HostApplication, Inbox, MemoryStream, context_ptr, handler_ptr,
    request, stream_ptr,
};
use super::processor::{Setup, Vst3Processor};
use super::shared::{Delivered, Objects, Shared};
use super::text::{ClassId, wide_text, wide_units};
use super::{
    FromAudio, MAX_BLOCK_SIZE, MAX_BUSES, MAX_CHANNELS_PER_BUS, MAX_LATENCY_FRAMES, MAX_PARAMS,
    MAX_SCRATCH_SAMPLES, ProcessFailure, QUEUE_CAPACITY, ToAudio, Vst3Error, Vst3Module, state,
};
use crate::editor::{EditorRequest, EditorSize, FdInterest, FdWatch, ParentWindow};

/// What a plugin says about one of its buses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusInfo {
    /// The name shown to people.
    pub name: String,
    /// How many channels the bus has: audio channels, or MIDI channels for
    /// an event bus.
    pub channels: u32,
    /// Whether this is a main bus. Any other is auxiliary: for an audio
    /// input, typically a sidechain.
    pub is_main: bool,
    /// Whether the plugin wants the bus switched on from the start.
    pub default_active: bool,
    /// For an audio bus, its speakers as VST 3 describes them, one bit per
    /// speaker (`SpeakerArrangement`); zero if the plugin does not say, and
    /// for an event bus.
    pub arrangement: u64,
    /// Whether the host has switched the bus on and the plugin accepted.
    ///
    /// VST 3 has every bus off until its host switches it on.
    /// [`Vst3Plugin::activate`] switches on the buses this host feeds or
    /// reads, and every bus the plugin wants on from the start, and
    /// switches the others off. A bus stays on after the plugin is
    /// deactivated. False before the first activation, and for a list read
    /// again after the plugin changed its buses.
    pub active: bool,
}

/// What may be done with a parameter, as the plugin declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct ParamFlags {
    /// May be automated.
    pub automatable: bool,
    /// The host may read it but not change it.
    pub read_only: bool,
    /// The value wraps around, like an angle.
    pub wrap_around: bool,
    /// The steps are a list of named choices.
    pub list: bool,
    /// Not to be shown to people.
    pub hidden: bool,
    /// Changing it changes the plugin's program.
    pub program_change: bool,
    /// This is the plugin's bypass switch.
    pub bypass: bool,
    /// Every flag as VST 3 defines it (`ParameterInfo::flags`).
    pub bits: i32,
}

impl ParamFlags {
    fn from_bits(bits: i32) -> Self {
        use vst::ParameterInfo_::ParameterFlags_ as flags;
        let has = |flag: i32| bits & flag != 0;
        Self {
            automatable: has(flags::kCanAutomate as i32),
            read_only: has(flags::kIsReadOnly as i32),
            wrap_around: has(flags::kIsWrapAround as i32),
            list: has(flags::kIsList as i32),
            hidden: has(flags::kIsHidden as i32),
            program_change: has(flags::kIsProgramChange as i32),
            bypass: has(flags::kIsBypass as i32),
            bits,
        }
    }
}

/// What a plugin says about one of its parameters.
///
/// VST 3 moves every parameter as a *normalized* value between 0 and 1:
/// that is the scale of [`default`](Self::default), of
/// [`Vst3Plugin::param_value`], of [`Vst3Plugin::set_param`] and of the
/// values in the events [`Vst3Processor`] takes. The plugin turns it into
/// what it means with [`Vst3Plugin::normalized_to_plain`] and
/// [`Vst3Plugin::param_to_text`].
#[derive(Debug, Clone, PartialEq)]
pub struct ParamInfo {
    /// The number that stands for the parameter in automation and every
    /// call here. It stays the same from session to session.
    pub id: ParamId,
    /// The name shown to people.
    pub title: String,
    /// A shorter name, for small displays.
    pub short_title: String,
    /// The unit of the plain value, such as `"dB"`.
    pub units: String,
    /// How many steps the value moves in, or zero for a continuous value.
    /// A switch has one.
    pub step_count: u32,
    /// The normalized value the parameter starts at, held to `0..=1`.
    pub default: f64,
    /// The unit the plugin files the parameter under, as VST 3 numbers
    /// them. The root unit is zero.
    pub unit_id: i32,
    /// What may be done with the parameter.
    pub flags: ParamFlags,
}

/// Something a plugin asked for or reported, as returned by
/// [`Vst3Plugin::pump`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum HostEvent {
    /// The plugin asks to be deactivated and activated again, because its
    /// buses, its latency or its controller mapping changed. Drop the
    /// [`Vst3Processor`], call [`Vst3Plugin::deactivate`], then
    /// [`Vst3Plugin::activate`]. `flags` are the plugin's `RestartFlags`
    /// that call for it.
    RestartRequested {
        /// The `RestartFlags` bits, as VST 3 defines them.
        flags: i32,
    },
    /// The plugin changed a parameter by itself: through its controller,
    /// as its own interface does, or from its processor. The value is
    /// normalized and held to `0..=1`. The host has passed it on to the
    /// other half of the plugin.
    ParamValue {
        /// The parameter.
        param: ParamId,
        /// Its new normalized value.
        value: f64,
    },
    /// A control in the plugin's interface was taken hold of.
    GestureBegin {
        /// The parameter.
        param: ParamId,
    },
    /// The control was let go.
    GestureEnd {
        /// The parameter.
        param: ParamId,
    },
    /// Something about the parameters changed, and the list has been read
    /// again.
    ParamsRescanned {
        /// Values changed without events. Read them with
        /// [`Vst3Plugin::param_value`].
        values: bool,
        /// Names, flags, or the list itself changed.
        info: bool,
    },
    /// The plugin's state differs from what was last saved or loaded.
    StateDirty,
    /// The plugin asked for its editor to be shown: open it with
    /// [`Vst3Plugin::open_editor`], or bring its window forward if it is
    /// open.
    ///
    /// Only the application can make the window an editor goes in, so the
    /// plugin was told its request was met only if the editor was open
    /// already. A request for a view other than the editor, the one view
    /// this host opens, is refused and remarked on instead.
    EditorRequested,
    /// The open editor asked the application for something; see
    /// [`EditorRequest`]. A VST 3 editor asks only for a new size.
    Editor(EditorRequest),
    /// The plugin's latency is now this many frames.
    LatencyChanged(u32),
    /// At least one block came out silent, for this reason, since the last
    /// call. [`Vst3Plugin::failed_blocks`] counts them.
    ProcessFailed(ProcessFailure),
    /// Something the host noticed about the plugin: a call on the wrong
    /// thread, a value that was refused, a part that could not be made.
    Remark(String),
}

/// How long a plugin goes on sounding after its input stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tail {
    /// This many frames.
    Frames(u32),
    /// For ever: an oscillator, a held note, a frozen reverb.
    Infinite,
}

/// The tail a plugin's answer to `getTailSamples` stands for.
pub(crate) fn tail_of(raw: u32) -> Tail {
    if raw == vst::kInfiniteTail {
        Tail::Infinite
    } else {
        Tail::Frames(raw)
    }
}

/// The main thread's ends of the two queues to the audio half.
struct Link {
    to_audio: Producer<ToAudio>,
    from_audio: Consumer<FromAudio>,
    delivered: Arc<Delivered>,
}

/// The plugin's open editor.
struct OpenEditor {
    view: ComPtr<IPlugView>,
    /// The view's content scale, if it takes one.
    scale_support: Option<ComPtr<IPlugViewContentScaleSupport>>,
    /// The frame the view was given. The plugin holds references of its
    /// own, which refuse everything once the editor is closed.
    _frame: ComWrapper<Frame>,
    /// The size the view last reported or was given.
    size: EditorSize,
}

/// The objects the main thread calls, as pointers that [`Shared`] owns.
#[derive(Clone, Copy)]
struct Raw {
    component: *mut IComponent,
    processor: *mut IAudioProcessor,
    controller: *mut IEditController,
    midi_mapping: *mut IMidiMapping,
}

/// A hosted VST 3 plugin as the main thread deals with it.
///
/// It owns everything VST 3 reserves for the main thread: buses,
/// parameters and their text, state, activation, the editor, and what the
/// plugin's controller reports to its host. Audio is processed by the
/// [`Vst3Processor`] that [`activate`](Self::activate) returns.
///
/// The value cannot leave the thread it was created on, and the compiler
/// enforces that:
///
/// ```compile_fail,E0277
/// fn can_be_sent<T: Send>() {}
/// can_be_sent::<manguebyte_host::vst3::Vst3Plugin>();
/// ```
///
/// The same check passes for the audio half and for the module:
///
/// ```
/// fn can_be_sent<T: Send>() {}
/// can_be_sent::<manguebyte_host::vst3::Vst3Processor>();
/// can_be_sent::<manguebyte_host::vst3::Vst3Module>();
/// ```
///
/// Call [`pump`](Self::pump) regularly, for instance from a timer: it is
/// where what the plugin reported is passed on.
///
/// The example in the [module documentation](super) takes a plugin through
/// all of this and runs as a test.
///
/// # Dropping
///
/// Dropping the plugin deactivates, disconnects, terminates and releases
/// it. If its audio half still exists, the plugin lives on until that is
/// dropped too. Should that happen on another thread after this value is
/// gone, the plugin can no longer be terminated where VST 3 requires, and
/// is left loaded instead: drop the audio half first.
pub struct Vst3Plugin {
    shared: Arc<Shared>,
    raw: Raw,
    class: ClassInfo,
    inbox: Arc<Inbox>,
    params: Vec<ParamInfo>,
    /// `(id, index into params)`, sorted by id.
    slots: Vec<(u32, u32)>,
    audio_inputs: Vec<BusInfo>,
    audio_outputs: Vec<BusInfo>,
    event_inputs: Vec<BusInfo>,
    event_outputs: Vec<BusInfo>,
    /// Why the plugin's buses cannot be served, if they cannot.
    layout_problem: Option<&'static str>,
    /// Whether the buses are to be read again once the plugin is inactive.
    buses_stale: bool,
    /// Audio output buses the caller switched on or off itself, by index,
    /// for the next activation.
    chosen_outputs: Vec<(usize, bool)>,
    link: Option<Link>,
    /// Parameter values for the plugin's processor that are waiting for it
    /// to be activated, latest last, one per parameter.
    pending: Vec<(u32, f64)>,
    /// Events gathered between calls to `pump`.
    waiting: Vec<HostEvent>,
    /// Most bytes a state stream moves per call.
    stream_chunk: usize,
    /// The open editor, if there is one.
    editor: Option<OpenEditor>,
    /// What the editor's frames record: its requests, timers and
    /// descriptors.
    editor_loop: Arc<EditorLoop>,
    /// Room for the ids of the timers due in one `pump`.
    due_timers: Vec<u32>,
    /// Keeps the value on the thread that made it.
    _not_send: PhantomData<*const ()>,
}

impl Vst3Plugin {
    /// Creates and initialises the plugin of `class`, with its controller.
    pub(crate) fn create(
        module: Vst3Module,
        class: ClassInfo,
        host_name: &str,
    ) -> Result<Self, Vst3Error> {
        let host = HostApplication::new(host_name);
        let inbox = Inbox::new();
        let handler = ComponentHandler::new(Arc::clone(&inbox));
        let component: ComPtr<IComponent> = module
            .create_instance(class.id)
            .ok_or(Vst3Error::CreateFailed(class.id))?;
        // SAFETY: a new component, initialised once, first, on the thread
        // that is from now on its main thread, with a host context that the
        // objects below keep alive for as long as the plugin.
        if unsafe { component.initialize(context_ptr(&host)) } != kResultOk {
            // Never initialised, so it is released without terminating.
            return Err(Vst3Error::InitializeFailed);
        }
        let Some(processor) = component.cast::<IAudioProcessor>() else {
            // SAFETY: initialised above, terminated once, on its main thread.
            unsafe { component.terminate() };
            return Err(Vst3Error::MissingInterface("IAudioProcessor"));
        };

        let mut remarks = Vec::new();
        // The controller: the same object, or one of its own.
        let (controller, separate_controller) = match component.cast::<IEditController>() {
            Some(controller) => (Some(controller), false),
            None => (
                make_controller(&module, &component, &host, &mut remarks),
                true,
            ),
        };
        let mut connection = None;
        if let (true, Some(controller)) = (separate_controller, &controller) {
            // Two objects talk through their connection points, each
            // connected to the other.
            if let (Some(theirs), Some(ours)) = (
                component.cast::<IConnectionPoint>(),
                controller.cast::<IConnectionPoint>(),
            ) {
                // SAFETY: two initialised objects of the plugin, connected
                // on their main thread, each with a pointer to the other
                // that `Objects` keeps alive until they are disconnected.
                unsafe {
                    theirs.connect(ours.as_ptr());
                    ours.connect(theirs.as_ptr());
                }
                connection = Some((theirs, ours));
            }
        }
        let midi_mapping = controller
            .as_ref()
            .and_then(|controller| controller.cast::<IMidiMapping>());

        // The handler is kept alive by `Objects` for as long as the plugin.
        let handler_raw = handler_ptr(&handler);
        let raw = Raw {
            component: component.as_ptr(),
            processor: processor.as_ptr(),
            controller: controller
                .as_ref()
                .map_or(std::ptr::null_mut(), ComPtr::as_ptr),
            midi_mapping: midi_mapping
                .as_ref()
                .map_or(std::ptr::null_mut(), ComPtr::as_ptr),
        };
        let shared = Shared::new(Objects {
            connection,
            _midi_mapping: midi_mapping,
            controller,
            separate_controller,
            _processor: processor,
            component,
            _handler: handler,
            _host: host,
            _module: module,
        });

        let mut this = Self {
            shared,
            raw,
            class,
            inbox,
            params: Vec::new(),
            slots: Vec::new(),
            audio_inputs: Vec::new(),
            audio_outputs: Vec::new(),
            event_inputs: Vec::new(),
            event_outputs: Vec::new(),
            layout_problem: None,
            buses_stale: false,
            chosen_outputs: Vec::new(),
            link: None,
            pending: Vec::new(),
            waiting: remarks.into_iter().map(HostEvent::Remark).collect(),
            stream_chunk: usize::MAX,
            editor: None,
            editor_loop: EditorLoop::new(),
            due_timers: Vec::new(),
            _not_send: PhantomData,
        };
        if this.has_controller() {
            // The controller learns the component's state, as VST 3 has a
            // host do after creating a plugin, and is given its handler.
            this.give_controller_component_state(None);
            if let Some(controller) = this.controller() {
                // SAFETY: the plugin's initialised controller, on its main
                // thread, with a handler `Objects` keeps alive for as long
                // as the plugin; it is taken back before the plugin is
                // terminated.
                unsafe { controller.setComponentHandler(handler_raw) };
            }
        }
        this.read_buses();
        this.read_params();
        Ok(this)
    }

    // --- The objects -------------------------------------------------------

    fn component(&self) -> ComRef<'_, IComponent> {
        // SAFETY: `Shared` owns a reference to the component and releases
        // it only once this value is gone.
        unsafe { ComRef::from_raw_unchecked(self.raw.component) }
    }

    fn processor(&self) -> ComRef<'_, IAudioProcessor> {
        // SAFETY: as for the component.
        unsafe { ComRef::from_raw_unchecked(self.raw.processor) }
    }

    fn controller(&self) -> Option<ComRef<'_, IEditController>> {
        // SAFETY: null, or owned by `Shared` as the component is.
        unsafe { ComRef::from_raw(self.raw.controller) }
    }

    fn mapping_object(&self) -> Option<ComRef<'_, IMidiMapping>> {
        // SAFETY: null, or owned by `Shared` as the component is.
        unsafe { ComRef::from_raw(self.raw.midi_mapping) }
    }

    /// Leaves a remark about the plugin for the next `pump`.
    fn remark(&mut self, message: String) {
        self.waiting.push(HostEvent::Remark(message));
    }

    // --- What the plugin is ------------------------------------------------

    /// What the module's factory said about the plugin's class.
    pub fn class(&self) -> &ClassInfo {
        &self.class
    }

    /// Whether the plugin has a controller, and so parameters.
    pub fn has_controller(&self) -> bool {
        !self.raw.controller.is_null()
    }

    /// The plugin's audio inputs, in the plugin's order. A main input, if
    /// there is one, is first.
    pub fn audio_inputs(&self) -> &[BusInfo] {
        &self.audio_inputs
    }

    /// The plugin's audio outputs, in the plugin's order.
    pub fn audio_outputs(&self) -> &[BusInfo] {
        &self.audio_outputs
    }

    /// The plugin's event inputs. Empty for a plugin that takes no notes.
    pub fn event_inputs(&self) -> &[BusInfo] {
        &self.event_inputs
    }

    /// The plugin's event outputs.
    pub fn event_outputs(&self) -> &[BusInfo] {
        &self.event_outputs
    }

    fn read_buses(&mut self) {
        self.layout_problem = None;
        self.buses_stale = false;
        self.audio_inputs = self.bus_list(MediaTypes_::kAudio, BusDirections_::kInput);
        self.audio_outputs = self.bus_list(MediaTypes_::kAudio, BusDirections_::kOutput);
        self.event_inputs = self.bus_list(MediaTypes_::kEvent, BusDirections_::kInput);
        self.event_outputs = self.bus_list(MediaTypes_::kEvent, BusDirections_::kOutput);
    }

    fn bus_list(&mut self, media: vst::MediaTypes, direction: vst::BusDirections) -> Vec<BusInfo> {
        let is_audio = media == MediaTypes_::kAudio;
        let (media, direction) = (media as vst::MediaType, direction as vst::BusDirection);
        // SAFETY: the plugin's initialised component, on its main thread.
        let declared =
            usize::try_from(unsafe { self.component().getBusCount(media, direction) }).unwrap_or(0);
        if is_audio && declared > MAX_BUSES {
            self.layout_problem = Some("more audio buses than this host serves");
        }
        let mut buses = Vec::new();
        for index in 0..declared.min(MAX_BUSES) as i32 {
            // SAFETY: every field is a number or an array of numbers, for
            // which all zeros is a value.
            let mut raw: vst::BusInfo = unsafe { std::mem::zeroed() };
            // SAFETY: as above, with an index below the count the plugin
            // reported and a struct this function owns.
            let found = unsafe {
                self.component()
                    .getBusInfo(media, direction, index, &raw mut raw)
            };
            if found != kResultOk {
                if is_audio {
                    // Its process call would still expect a buffer for it.
                    self.layout_problem =
                        Some("an audio bus the plugin counts but does not describe");
                }
                break;
            }
            let channels = u32::try_from(raw.channelCount).unwrap_or(0);
            if is_audio && channels as usize > MAX_CHANNELS_PER_BUS {
                self.layout_problem = Some("an audio bus with more channels than this host serves");
            }
            let mut arrangement = 0u64;
            if is_audio {
                // SAFETY: the plugin's processor, on its main thread, with
                // a number this function owns.
                let given = unsafe {
                    self.processor()
                        .getBusArrangement(direction, index, &raw mut arrangement)
                };
                if given != kResultOk {
                    arrangement = 0;
                }
            }
            buses.push(BusInfo {
                name: wide_text(&raw.name),
                channels,
                is_main: raw.busType == BusTypes_::kMain as vst::BusType,
                default_active: raw.flags & vst::BusInfo_::BusFlags_::kDefaultActive as u32 != 0,
                arrangement,
                active: false,
            });
        }
        buses
    }

    // --- Parameters --------------------------------------------------------

    /// The plugin's parameters, in the plugin's order.
    ///
    /// At most [`MAX_PARAMS`] are listed. A parameter whose description
    /// cannot be read, or whose id is VST 3's "no parameter" or repeats an
    /// earlier one, is left out and remarked on by [`pump`](Self::pump).
    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    /// The parameter with this id, if the plugin has one.
    pub fn param(&self, id: ParamId) -> Option<&ParamInfo> {
        let index = self.slot(id)?;
        self.params.get(index)
    }

    fn slot(&self, id: ParamId) -> Option<usize> {
        self.slots
            .binary_search_by_key(&id.0, |&(slot, _)| slot)
            .ok()
            .map(|at| self.slots[at].1 as usize)
    }

    fn read_params(&mut self) {
        self.params.clear();
        self.slots.clear();
        let Some(controller) = self.controller() else {
            return;
        };
        // SAFETY: the plugin's initialised controller, on its main thread.
        let declared = usize::try_from(unsafe { controller.getParameterCount() }).unwrap_or(0);
        let mut params = Vec::new();
        let mut refused = 0usize;
        for index in 0..declared.min(MAX_PARAMS) as i32 {
            // SAFETY: every field is a number or an array of numbers.
            let mut raw: vst::ParameterInfo = unsafe { std::mem::zeroed() };
            // SAFETY: as above, with an index below the count the plugin
            // reported and a struct this function owns.
            if unsafe { controller.getParameterInfo(index, &raw mut raw) } != kResultOk
                || raw.id == vst::kNoParamId
            {
                refused += 1;
                continue;
            }
            params.push(ParamInfo {
                id: ParamId(raw.id),
                title: wide_text(&raw.title),
                short_title: wide_text(&raw.shortTitle),
                units: wide_text(&raw.units),
                step_count: u32::try_from(raw.stepCount).unwrap_or(0),
                default: unit(raw.defaultNormalizedValue),
                unit_id: raw.unitId,
                flags: ParamFlags::from_bits(raw.flags),
            });
        }
        // Two parameters with one id cannot be told apart, so only the first
        // is kept.
        let mut seen = std::collections::HashSet::new();
        let before = params.len();
        params.retain(|param| seen.insert(param.id.0));
        refused += before - params.len();
        if declared > MAX_PARAMS {
            self.remark(format!(
                "the plugin declares {declared} parameters; the first {MAX_PARAMS} are used"
            ));
        }
        if refused > 0 {
            self.remark(format!(
                "{refused} parameter(s) were left out: no description, no id, or an id \
                 already taken"
            ));
        }
        self.slots = params
            .iter()
            .enumerate()
            .map(|(index, param)| (param.id.0, index as u32))
            .collect();
        self.slots.sort_unstable();
        self.params = params;
    }

    /// The current normalized value of a parameter, as the plugin's
    /// controller reports it, held to `0..=1`.
    ///
    /// The host keeps the controller up to date with what it sends the
    /// plugin's processor, at every [`pump`](Self::pump), as VST 3 hosts
    /// do; a value delivered while audio runs shows up here after the next
    /// one.
    ///
    /// Returns `None` if the plugin has no such parameter, or answers with
    /// something that is not a number.
    pub fn param_value(&self, id: ParamId) -> Option<f64> {
        self.slot(id)?;
        let controller = self.controller()?;
        // SAFETY: the plugin's initialised controller, on its main thread.
        let value = unsafe { controller.getParamNormalized(id.0) };
        value.is_finite().then(|| unit(value))
    }

    /// Sets a parameter to a normalized value, held to `0..=1`.
    ///
    /// The controller is told at once. The processor hears it as a point on
    /// the first frame of the next block, ahead of that block's own events:
    /// through a wait-free queue while an audio half exists, and otherwise
    /// in the first block after the next activation.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::UnknownParam`] for an id the plugin does not have,
    /// [`Vst3Error::InvalidArgument`] for a value that is not a number, and
    /// [`Vst3Error::QueueFull`] if 1,024 changes are already waiting for the
    /// audio thread.
    pub fn set_param(&mut self, id: ParamId, value: f64) -> Result<(), Vst3Error> {
        self.slot(id).ok_or(Vst3Error::UnknownParam(id))?;
        if value.is_nan() {
            return Err(Vst3Error::InvalidArgument(
                "a parameter value has to be a number",
            ));
        }
        let value = unit(value);
        self.send(id.0, value)?;
        if let Some(controller) = self.controller() {
            // SAFETY: the plugin's initialised controller, on its main
            // thread.
            unsafe { controller.setParamNormalized(id.0, value) };
        }
        Ok(())
    }

    /// Hands a value to the processor: through the queue while an audio
    /// half exists, or to the list for the next activation.
    fn send(&mut self, id: u32, value: f64) -> Result<(), Vst3Error> {
        if self.shared.processor_alive() {
            if let Some(link) = &mut self.link {
                let message = ToAudio::ParamValue {
                    id,
                    value,
                    generation: self.shared.generation(),
                };
                return link
                    .to_audio
                    .push(message)
                    .map_err(|_| Vst3Error::QueueFull);
            }
        }
        let unread = self.retire_link();
        for (id, value) in unread.into_iter().chain([(id, value)]) {
            self.pending.retain(|&(waiting, _)| waiting != id);
            self.pending.push((id, value));
        }
        Ok(())
    }

    /// The plain value, in the parameter's own unit, that a normalized
    /// value stands for, as the plugin works it out.
    ///
    /// Returns `None` if the plugin has no such parameter, or answers with
    /// something that is not a number.
    pub fn normalized_to_plain(&self, id: ParamId, value: f64) -> Option<f64> {
        self.slot(id)?;
        let controller = self.controller()?;
        // SAFETY: the plugin's initialised controller, on its main thread.
        let plain = unsafe { controller.normalizedParamToPlain(id.0, unit(value)) };
        plain.is_finite().then_some(plain)
    }

    /// The normalized value that a plain value stands for, as the plugin
    /// works it out, held to `0..=1`.
    ///
    /// Returns `None` if the plugin has no such parameter, if `plain` is not
    /// a number, or if the plugin answers with something that is not one.
    pub fn plain_to_normalized(&self, id: ParamId, plain: f64) -> Option<f64> {
        self.slot(id)?;
        if plain.is_nan() {
            return None;
        }
        let controller = self.controller()?;
        // SAFETY: the plugin's initialised controller, on its main thread.
        let value = unsafe { controller.plainParamToNormalized(id.0, plain) };
        value.is_finite().then(|| unit(value))
    }

    /// The text a person should see for a normalized value of a parameter,
    /// as the plugin words it. At most 127 characters.
    ///
    /// Returns `None` if the plugin has no such parameter or cannot say, or
    /// if `value` is not a number.
    pub fn param_to_text(&self, id: ParamId, value: f64) -> Option<String> {
        self.slot(id)?;
        if value.is_nan() {
            return None;
        }
        let controller = self.controller()?;
        let mut text: vst::String128 = [0; 128];
        // SAFETY: the plugin's initialised controller, on its main thread,
        // with a buffer of the size VST 3 sets for this call.
        if unsafe { controller.getParamStringByValue(id.0, unit(value), &raw mut text) }
            != kResultOk
        {
            return None;
        }
        Some(wide_text(&text))
    }

    /// The normalized value a piece of text stands for, as the plugin reads
    /// it: the reverse of [`param_to_text`](Self::param_to_text), held to
    /// `0..=1`.
    ///
    /// Returns `None` if the plugin has no such parameter, cannot make sense
    /// of the text, or answers with something that is not a number.
    pub fn text_to_param(&self, id: ParamId, text: &str) -> Option<f64> {
        self.slot(id)?;
        let controller = self.controller()?;
        let mut units = wide_units(text);
        let mut value = f64::NAN;
        // SAFETY: the plugin's initialised controller, on its main thread,
        // with zero-terminated text and a number this function owns.
        if unsafe { controller.getParamValueByString(id.0, units.as_mut_ptr(), &raw mut value) }
            != kResultOk
        {
            return None;
        }
        value.is_finite().then(|| unit(value))
    }

    /// The parameter the plugin maps a MIDI controller of a channel to, if
    /// it maps it to one. `controller` is a MIDI controller number, or 128
    /// for channel pressure and 129 for pitch bend, as VST 3 numbers them.
    pub fn midi_mapping(&self, channel: u8, controller: u16) -> Option<ParamId> {
        let mapping = self.mapping_object()?;
        let controller = i16::try_from(controller).ok()?;
        let mut id = vst::kNoParamId;
        // SAFETY: the plugin's initialised controller, on its main thread,
        // with a number this function owns.
        let found = unsafe {
            mapping.getMidiControllerAssignment(0, i16::from(channel), controller, &raw mut id)
        };
        (found == kResultOk && id != vst::kNoParamId).then_some(ParamId(id))
    }

    /// Asks the plugin for its whole table of controller mappings, as the
    /// audio half uses it.
    fn midi_map(&self) -> Option<Box<MidiMap>> {
        self.mapping_object()?;
        self.event_inputs.first()?;
        let mut map = Box::new([[vst::kNoParamId; CONTROLLERS]; 16]);
        for (channel, controllers) in map.iter_mut().enumerate() {
            for (controller, slot) in controllers.iter_mut().enumerate() {
                if let Some(id) = self.midi_mapping(channel as u8, controller as u16) {
                    *slot = id.0;
                }
            }
        }
        Some(map)
    }

    // --- State -------------------------------------------------------------

    /// Asks the plugin for everything it would need to come back as it is
    /// now: its component's state and its controller's, in one run of
    /// bytes that also names the plugin's class.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::StateSaveFailed`] if the component reports failure, and
    /// [`Vst3Error::StateTooLarge`] if either part is larger than
    /// [`MAX_STATE_BYTES`](super::MAX_STATE_BYTES). A controller that cannot
    /// save is saved as empty.
    pub fn save_state(&mut self) -> Result<Vec<u8>, Vst3Error> {
        let stream = MemoryStream::empty(self.stream_chunk);
        // SAFETY: the plugin's initialised component, on its main thread,
        // with a stream that outlives the call.
        let saved = unsafe { self.component().getState(stream_ptr(&stream)) };
        let component = stream.finish().ok_or(Vst3Error::StateTooLarge)?;
        if saved != kResultOk {
            return Err(Vst3Error::StateSaveFailed);
        }
        let mut controller_state = Vec::new();
        if let Some(controller) = self.controller() {
            let stream = MemoryStream::empty(self.stream_chunk);
            // SAFETY: the plugin's initialised controller, as above.
            let saved = unsafe { controller.getState(stream_ptr(&stream)) };
            let bytes = stream.finish().ok_or(Vst3Error::StateTooLarge)?;
            if saved == kResultOk {
                controller_state = bytes;
            }
        }
        Ok(state::encode(self.class.id, &component, &controller_state))
    }

    /// Gives the plugin back a state saved by [`save_state`](Self::save_state):
    /// the component's part to the component, then to the controller as
    /// the component's state, then the controller's own part.
    ///
    /// The state has the last word over every parameter value that is on
    /// its way when it is loaded. First, what happened up to the call is
    /// passed on as [`pump`](Self::pump) passes it on: the controller is
    /// told what the processor was sent and what it reported, and the next
    /// `pump` returns what the processor and the controller reported. Then
    /// every value the processor has not yet heard is dropped: values set
    /// with [`set_param`](Self::set_param) or edited in the controller,
    /// whether they wait for the audio half or for the next activation.
    /// Nothing sent before the load reaches the plugin or its controller
    /// after it.
    ///
    /// A block that an audio half is processing at the very moment of the
    /// load may fall on either side of it, for the host as for the plugin.
    /// Load a state between blocks, or with no audio half, to be sure of
    /// the outcome.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::NotAState`] if the bytes are not a state this host
    /// saved and [`Vst3Error::StateOfAnotherClass`] if they belong to
    /// another plugin; in both cases nothing is changed.
    /// [`Vst3Error::StateLoadFailed`] if the component refuses its part.
    /// The values on their way are dropped all the same, and what the
    /// plugin has changed by the time it refuses is up to the plugin. A
    /// controller that refuses its own part is remarked on by the next
    /// [`pump`](Self::pump).
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), Vst3Error> {
        let parts = state::decode(bytes).map_err(Vst3Error::NotAState)?;
        if parts.class != self.class.id {
            return Err(Vst3Error::StateOfAnotherClass(parts.class));
        }
        // What happened up to now goes where it belongs first, in the order
        // it happened; then the state has the last word over everything
        // still on its way.
        self.pass_on();
        self.supersede();
        let stream = MemoryStream::holding(parts.component.to_vec(), self.stream_chunk);
        // SAFETY: the plugin's initialised component, on its main thread,
        // with a stream that outlives the call.
        if unsafe { self.component().setState(stream_ptr(&stream)) } != kResultOk {
            return Err(Vst3Error::StateLoadFailed);
        }
        self.give_controller_component_state(Some(parts.component));
        if let (Some(controller), false) = (self.controller(), parts.controller.is_empty()) {
            let stream = MemoryStream::holding(parts.controller.to_vec(), self.stream_chunk);
            // SAFETY: the plugin's initialised controller, as above.
            let loaded = unsafe { controller.setState(stream_ptr(&stream)) };
            if loaded != kResultOk && loaded != kNotImplemented {
                self.remark("the plugin's controller refused its part of the state".to_owned());
            }
        }
        Ok(())
    }

    /// Drops every parameter value still on its way to the processor or
    /// back from it, because a state is about to replace them all.
    fn supersede(&mut self) {
        // From here on, whatever was sent before is of an older generation:
        // the audio half does not deliver it, and the main thread does not
        // tell the controller about it.
        self.shared.next_generation();
        self.pending.clear();
        // An audio half that is gone has nothing more to deliver, so the
        // link to it is let go now. What it never read is of the old
        // generation, so nothing comes back to be kept.
        let _replaced = self.retire_link();
    }

    /// Hands the controller the component's state, as VST 3 has a host do
    /// after creating a plugin and after loading a state: `component` if
    /// given, or what the component saves now.
    fn give_controller_component_state(&mut self, component: Option<&[u8]>) {
        let Some(controller) = self.controller() else {
            return;
        };
        let stream = match component {
            Some(bytes) => MemoryStream::holding(bytes.to_vec(), self.stream_chunk),
            None => {
                let stream = MemoryStream::empty(self.stream_chunk);
                // SAFETY: the plugin's initialised component, on its main
                // thread, with a stream that outlives the call.
                if unsafe { self.component().getState(stream_ptr(&stream)) } != kResultOk {
                    return;
                }
                stream.rewind();
                stream
            }
        };
        // SAFETY: the plugin's initialised controller, as above.
        let taken = unsafe { controller.setComponentState(stream_ptr(&stream)) };
        if taken != kResultOk && taken != kNotImplemented {
            self.remark("the plugin's controller refused the component's state".to_owned());
        }
    }

    /// Limits how many bytes the state streams move in one call.
    ///
    /// VST 3 lets a stream move fewer bytes than a plugin asks for, and a
    /// plugin has to ask again. Setting a small limit here shows whether a
    /// plugin copes. Zero means no limit, which is the default.
    pub fn set_stream_chunk(&mut self, bytes: usize) {
        self.stream_chunk = if bytes == 0 { usize::MAX } else { bytes };
    }

    // --- Latency and tail --------------------------------------------------

    /// How many frames late the plugin's output is, as it reported when it
    /// was last activated. Zero before the first activation. At most
    /// [`MAX_LATENCY_FRAMES`].
    pub fn latency(&self) -> u32 {
        self.shared.latency()
    }

    /// How long the plugin goes on sounding after its input stops, as the
    /// plugin says now.
    ///
    /// VST 3 has a plugin asked for its tail on the main thread only. The
    /// answer is also what [`Vst3Processor::tail`] reports from then on;
    /// [`activate`](Self::activate) and [`pump`](Self::pump) ask too.
    pub fn tail(&self) -> Tail {
        // SAFETY: the plugin's initialised processor, on its main thread.
        let raw = unsafe { self.processor().getTailSamples() };
        self.shared.set_tail(raw);
        tail_of(raw)
    }

    // --- Activation --------------------------------------------------------

    /// Whether the plugin is active.
    pub fn is_active(&self) -> bool {
        self.shared.active()
    }

    /// Whether the audio half made by the last activation still exists.
    pub fn has_processor(&self) -> bool {
        self.shared.processor_alive()
    }

    /// Gets the plugin ready to process at `sample_rate`, in blocks of up
    /// to `max_block_size` frames, and returns its audio half.
    ///
    /// The plugin keeps its own speaker arrangements. The buses this host
    /// feeds or reads are switched on: the main audio input and output,
    /// the first auxiliary audio input (the sidechain), the first event
    /// input and the first event output. So is every other bus the plugin
    /// wants on from the start, which is given silence or whose output is
    /// discarded; every remaining bus is switched off.
    /// [`BusInfo::active`] records the outcome, and a bus the plugin
    /// refuses to switch on is remarked on by the next [`pump`](Self::pump).
    /// The plugin is asked to process 32-bit samples in real time, and its
    /// latency and tail are read once it is active.
    ///
    /// Everything processing will need is allocated here: a buffer for
    /// every channel of every bus, the tables VST 3 wants them described
    /// by, the parameter changes and event lists of a block in both
    /// directions, the table of controller mappings, and the two queues
    /// between the halves.
    ///
    /// # Errors
    ///
    /// - [`Vst3Error::AlreadyActive`] if the plugin is active.
    /// - [`Vst3Error::InvalidArgument`] for a sample rate that is not a
    ///   positive number, or a block size of zero or above
    ///   [`MAX_BLOCK_SIZE`].
    /// - [`Vst3Error::Unsupported`] if the plugin's buses are beyond what
    ///   this host serves, or it cannot process 32-bit samples.
    /// - [`Vst3Error::SetupRefused`] or [`Vst3Error::ActivationFailed`] if
    ///   the plugin refuses.
    pub fn activate(
        &mut self,
        sample_rate: f64,
        max_block_size: usize,
    ) -> Result<Vst3Processor, Vst3Error> {
        if self.shared.active() {
            return Err(Vst3Error::AlreadyActive);
        }
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            return Err(Vst3Error::InvalidArgument(
                "the sample rate has to be a positive number",
            ));
        }
        if max_block_size == 0 || max_block_size > MAX_BLOCK_SIZE {
            return Err(Vst3Error::InvalidArgument(
                "the block size has to be between 1 and MAX_BLOCK_SIZE",
            ));
        }
        if self.buses_stale {
            self.read_buses();
        }
        if let Some(problem) = self.layout_problem {
            return Err(Vst3Error::Unsupported(problem));
        }
        let channels: usize = self
            .audio_inputs
            .iter()
            .chain(&self.audio_outputs)
            .map(|bus| bus.channels as usize)
            .sum();
        if channels.saturating_mul(max_block_size) > MAX_SCRATCH_SAMPLES {
            return Err(Vst3Error::Unsupported(
                "more channels at this block size than this host allocates for",
            ));
        }
        let main_input = self
            .audio_inputs
            .first()
            .filter(|bus| bus.is_main)
            .map(|_| 0);
        let sidechain = (0..self.audio_inputs.len()).find(|&index| Some(index) != main_input);

        let single = vst::SymbolicSampleSizes_::kSample32 as i32;
        // SAFETY: the plugin's initialised, inactive processor, on its main
        // thread, with arrays this function owns that outlive each call.
        unsafe {
            let processor = self.processor();
            let supported = processor.canProcessSampleSize(single);
            if supported != kResultOk && supported != kResultTrue {
                return Err(Vst3Error::Unsupported(
                    "the plugin cannot process 32-bit samples",
                ));
            }
            // The plugin's own arrangements, which it is held to. A plugin
            // that refuses even those is processed as it stands, and so is
            // one that would not say what one of them is: no arrangement is
            // made up for it.
            let mut inputs: Vec<u64> = self
                .audio_inputs
                .iter()
                .map(|bus| bus.arrangement)
                .collect();
            let mut outputs: Vec<u64> = self
                .audio_outputs
                .iter()
                .map(|bus| bus.arrangement)
                .collect();
            let known = self
                .audio_inputs
                .iter()
                .chain(&self.audio_outputs)
                .all(|bus| bus.arrangement != 0 || bus.channels == 0);
            if known {
                processor.setBusArrangements(
                    inputs.as_mut_ptr(),
                    inputs.len() as i32,
                    outputs.as_mut_ptr(),
                    outputs.len() as i32,
                );
            }
        }
        self.switch_buses(main_input, sidechain);
        let mut setup = vst::ProcessSetup {
            processMode: vst::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: single,
            maxSamplesPerBlock: max_block_size as i32,
            sampleRate: sample_rate,
        };
        // SAFETY: as above, with a struct this function owns.
        if unsafe { self.processor().setupProcessing(&raw mut setup) } != kResultOk {
            return Err(Vst3Error::SetupRefused);
        }
        let midi_map = self.midi_map();
        // SAFETY: the plugin's initialised, inactive component, set up for
        // processing, on its main thread.
        if unsafe { self.component().setActive(1) } != kResultOk {
            return Err(Vst3Error::ActivationFailed);
        }
        self.shared.set_active(true);
        self.read_latency();
        // Read here, on the main thread, for the audio half to report.
        self.tail();

        let (mut to_audio, from_main) = spsc::channel(QUEUE_CAPACITY);
        let (to_main, from_audio) = spsc::channel(QUEUE_CAPACITY);
        // Values set while the plugin was inactive go first.
        let generation = self.shared.generation();
        for (id, value) in self.pending.drain(..) {
            let message = ToAudio::ParamValue {
                id,
                value,
                generation,
            };
            if to_audio.push(message).is_err() {
                self.shared.lost_events.fetch_add(1, Ordering::Relaxed);
            }
        }
        let delivered = Delivered::new(self.params.iter().map(|param| param.id.0).collect());
        self.link = Some(Link {
            to_audio,
            from_audio,
            delivered: Arc::clone(&delivered),
        });
        Ok(Vst3Processor::new(Setup {
            shared: Arc::clone(&self.shared),
            processor: self.raw.processor,
            sample_rate,
            max_frames: max_block_size,
            inputs: self.audio_inputs.iter().map(|bus| bus.channels).collect(),
            outputs: self.audio_outputs.iter().map(|bus| bus.channels).collect(),
            main_input,
            sidechain,
            has_event_input: !self.event_inputs.is_empty(),
            has_event_output: !self.event_outputs.is_empty(),
            delivered,
            midi_map,
            from_main,
            to_main,
        }))
    }

    /// Switches the plugin's buses on or off for an activation, as
    /// [`activate`](Self::activate) describes, and records the outcome in
    /// [`BusInfo::active`].
    fn switch_buses(&mut self, main_input: Option<usize>, sidechain: Option<usize>) {
        let kinds = [
            (MediaTypes_::kAudio, BusDirections_::kInput, "audio input"),
            (MediaTypes_::kAudio, BusDirections_::kOutput, "audio output"),
            (MediaTypes_::kEvent, BusDirections_::kInput, "event input"),
            (MediaTypes_::kEvent, BusDirections_::kOutput, "event output"),
        ];
        for (media, direction, what) in kinds {
            let audio_inputs = media == MediaTypes_::kAudio && direction == BusDirections_::kInput;
            for index in 0..self.buses_mut(media, direction).len() {
                // What this host feeds or reads: among the audio inputs the
                // main one and the sidechain, of every other kind the first.
                let used = if audio_inputs {
                    Some(index) == main_input || Some(index) == sidechain
                } else {
                    index == 0
                };
                let mut wanted = used || self.buses_mut(media, direction)[index].default_active;
                let audio_outputs =
                    media == MediaTypes_::kAudio && direction == BusDirections_::kOutput;
                if let Some(&(_, chosen)) = self
                    .chosen_outputs
                    .iter()
                    .find(|&&(at, _)| audio_outputs && at == index)
                {
                    wanted = chosen;
                }
                // SAFETY: the plugin's initialised, inactive component, on
                // its main thread, with an index below the bus count it
                // reported.
                let answer = unsafe {
                    self.component().activateBus(
                        media as vst::MediaType,
                        direction as vst::BusDirection,
                        index as i32,
                        u8::from(wanted),
                    )
                };
                // A plugin that does not implement the call processes every
                // bus it has.
                let on = wanted && (answer == kResultOk || answer == kNotImplemented);
                self.buses_mut(media, direction)[index].active = on;
                if wanted && !on {
                    self.remark(format!(
                        "the plugin refused to switch on its {what} bus {index}"
                    ));
                }
            }
        }
    }

    /// Has audio output bus `index` switched on or off from the next
    /// [`activate`](Self::activate), in place of what `activate` would
    /// choose: an auxiliary output the plugin wants off from the start can
    /// be heard, and one it wants on can be spared. A bus that is off is
    /// silent in
    /// [`Vst3Processor::process_with_aux`](super::Vst3Processor::process_with_aux).
    ///
    /// # Errors
    ///
    /// - [`Vst3Error::AlreadyActive`] while the plugin is active.
    /// - [`Vst3Error::InvalidArgument`] for a bus the plugin lacks, and for
    ///   the main output, which this host always reads.
    pub fn set_audio_output_active(&mut self, index: usize, active: bool) -> Result<(), Vst3Error> {
        if self.shared.active() {
            return Err(Vst3Error::AlreadyActive);
        }
        if index == 0 || index >= self.audio_outputs.len() {
            return Err(Vst3Error::InvalidArgument(
                "no such auxiliary audio output bus",
            ));
        }
        self.chosen_outputs.retain(|&(at, _)| at != index);
        self.chosen_outputs.push((index, active));
        Ok(())
    }

    /// The list of buses of one kind and direction.
    fn buses_mut(
        &mut self,
        media: vst::MediaTypes,
        direction: vst::BusDirections,
    ) -> &mut Vec<BusInfo> {
        match (
            media == MediaTypes_::kAudio,
            direction == BusDirections_::kInput,
        ) {
            (true, true) => &mut self.audio_inputs,
            (true, false) => &mut self.audio_outputs,
            (false, true) => &mut self.event_inputs,
            (false, false) => &mut self.event_outputs,
        }
    }

    /// Reads the plugin's latency, which VST 3 has valid once it is active.
    fn read_latency(&mut self) {
        // SAFETY: the plugin's active processor, on its main thread.
        let reported = unsafe { self.processor().getLatencySamples() };
        if reported > MAX_LATENCY_FRAMES {
            self.remark(format!(
                "the plugin reports a latency of {reported} frames; {MAX_LATENCY_FRAMES} is used"
            ));
        }
        let frames = reported.min(MAX_LATENCY_FRAMES);
        if frames != self.shared.latency() {
            self.shared.set_latency(frames);
            self.waiting.push(HostEvent::LatencyChanged(frames));
        }
    }

    /// Deactivates the plugin, once its audio half has been dropped.
    ///
    /// Parameter changes the audio half had not yet read are not lost,
    /// unless a state loaded since has replaced them (see
    /// [`load_state`](Self::load_state)). The controller is told them again
    /// once the plugin is inactive, which is where a plugin whose
    /// controller and component are one object takes them in at once, and
    /// the processor hears them on the first frame after the next
    /// activation. Does nothing if the plugin is not active.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::ProcessorAlive`] if the [`Vst3Processor`] still exists:
    /// the plugin may be processing, and VST 3 forbids deactivating it then.
    pub fn deactivate(&mut self) -> Result<(), Vst3Error> {
        if !self.shared.active() {
            return Ok(());
        }
        if self.shared.processor_alive() {
            return Err(Vst3Error::ProcessorAlive);
        }
        let unread = self.retire_link();
        // SAFETY: the plugin's active component, on its main thread; the
        // audio half told it to stop processing when it was dropped.
        unsafe { self.component().setActive(0) };
        self.shared.set_active(false);
        for (id, value) in unread {
            self.tell_controller(id, value);
            self.pending.retain(|&(waiting, _)| waiting != id);
            self.pending.push((id, value));
        }
        if self.buses_stale {
            self.read_buses();
        }
        Ok(())
    }

    /// Lets go of the queues to an audio half that no longer exists, passes
    /// on what it reported, and returns the parameter changes it never
    /// read, leaving out the ones a state loaded since has replaced.
    fn retire_link(&mut self) -> Vec<(u32, f64)> {
        let mut unread = Vec::new();
        if self.shared.processor_alive() {
            return unread;
        }
        let Some(mut link) = self.link.take() else {
            return unread;
        };
        self.drain(&mut link);
        if let Some(mut queue) = self.shared.take_leftover() {
            let current = self.shared.generation();
            while let Some(ToAudio::ParamValue {
                id,
                value,
                generation,
            }) = queue.pop()
            {
                if generation >= current {
                    unread.push((id, value));
                }
            }
        }
        unread
    }

    /// Passes on what the audio half sent the processor and what the
    /// processor reported: both to the controller, and the second to the
    /// caller as well. What a state loaded since has replaced is dropped.
    fn drain(&mut self, link: &mut Link) {
        let current = self.shared.generation();
        link.delivered
            .take(current, |id, value| self.tell_controller(id, value));
        while let Some(FromAudio::ParamValue {
            id,
            value,
            generation,
        }) = link.from_audio.pop()
        {
            if generation < current {
                continue;
            }
            self.tell_controller(id, value);
            self.waiting.push(HostEvent::ParamValue {
                param: ParamId(id),
                value,
            });
        }
    }

    /// Tells the controller what the processor has, as a VST 3 host does
    /// with automation it plays and with what a processor reports.
    fn tell_controller(&self, id: u32, value: f64) {
        if let Some(controller) = self.controller() {
            // SAFETY: the plugin's initialised controller, on its main
            // thread.
            unsafe { controller.setParamNormalized(id, value) };
        }
    }

    // --- The editor --------------------------------------------------------

    /// Whether the plugin may have an editor: it has a controller, which is
    /// what makes one. Whether it really has one is known only once
    /// [`open_editor`](Self::open_editor) asks for it.
    pub fn has_editor(&self) -> bool {
        self.has_controller()
    }

    /// Opens the plugin's editor inside `parent`, a window of the
    /// application's, and returns the editor's size.
    ///
    /// The controller is asked for its `"editor"` view. The view is asked
    /// whether it can live in a window of the parent's kind, is given the
    /// host's frame, then `scale` if it takes a content scale, and is then
    /// attached to the window, which puts it on screen: VST 3 has no call
    /// that shows a view. Make the window the size returned.
    ///
    /// `scale` is the factor a user interface is drawn at on the parent's
    /// screen, such as 1.5 for 144 dots per inch on Windows. `None` leaves
    /// the plugin to find its own. A plugin that refuses the scale, as
    /// every plugin made with this framework does on macOS, where sizes
    /// are points that the system scales, opens at its own.
    ///
    /// From then on, the editor's requests are among the events that
    /// [`pump`](Self::pump) returns, and its timers fire there; see
    /// [`next_editor_timer`](Self::next_editor_timer). What the person does
    /// with its controls comes back from `pump` as the plugin's own edits,
    /// and reaches the processor at the start of its next block.
    ///
    /// # Errors
    ///
    /// Nothing is left open after an error.
    ///
    /// - [`Vst3Error::EditorAlreadyOpen`] if the editor is open.
    /// - [`Vst3Error::InvalidArgument`] for a scale that is not a positive
    ///   number.
    /// - [`Vst3Error::NoController`] if the plugin has no controller, and
    ///   [`Vst3Error::NoView`] if its controller makes no editor.
    /// - [`Vst3Error::PlatformUnsupported`] if the editor cannot be put in
    ///   a window of the parent's kind.
    /// - [`Vst3Error::EditorRefused`] if the view refuses the frame, refuses
    ///   to be attached, or then cannot say its size.
    pub fn open_editor(
        &mut self,
        parent: ParentWindow,
        scale: Option<f64>,
    ) -> Result<EditorSize, Vst3Error> {
        if self.editor.is_some() {
            return Err(Vst3Error::EditorAlreadyOpen);
        }
        if let Some(scale) = scale {
            check_scale(scale)?;
        }
        let api = parent.api();
        let (kind, handle) = platform_of(parent).ok_or(Vst3Error::PlatformUnsupported(api))?;
        let view = {
            let controller = self.controller().ok_or(Vst3Error::NoController)?;
            // SAFETY: the plugin's initialised controller, on its main
            // thread, with a name the bindings end in a zero. The view, if
            // any, comes with one reference, which is ours.
            unsafe { ComPtr::from_raw(controller.createView(vst::ViewType::kEditor)) }
        };
        let view = view.ok_or(Vst3Error::NoView)?;
        // SAFETY: the plugin's view, on its main thread, with a platform
        // type the bindings end in a zero.
        if unsafe { view.isPlatformTypeSupported(kind) } != kResultTrue {
            return Err(Vst3Error::PlatformUnsupported(api));
        }
        let frame = self.editor_loop.begin();
        // SAFETY: as above, with a frame that stays valid for as long as
        // the plugin holds a reference to it.
        if unsafe { view.setFrame(frame_ptr(&frame)) } != kResultOk {
            self.abandon(view, false);
            return Err(Vst3Error::EditorRefused("setFrame"));
        }
        let scale_support = view.cast::<IPlugViewContentScaleSupport>();
        if let (Some(scale), Some(support)) = (scale, &scale_support) {
            // SAFETY: the view, as above. A refusal leaves the plugin at its
            // own scale, as the documentation says.
            unsafe { support.setContentScaleFactor(scale as f32) };
        }
        // SAFETY: the view, as above, with the parent's handle, which the
        // caller says is a window of the kind named.
        if unsafe { view.attached(handle, kind) } != kResultOk {
            self.abandon(view, false);
            return Err(Vst3Error::EditorRefused("attached"));
        }
        let Some(size) = view_size(&view) else {
            self.abandon(view, true);
            return Err(Vst3Error::EditorRefused("getSize"));
        };
        self.editor = Some(OpenEditor {
            view,
            scale_support,
            _frame: frame,
            size,
        });
        self.inbox.set_editor_open(true);
        Ok(size)
    }

    /// Takes a view apart that is being closed or failed to open: removed
    /// from its window if it was attached, its frame taken back, what it
    /// registered forgotten, and released, in the order VST 3 has a host
    /// do it.
    fn abandon(&self, view: ComPtr<IPlugView>, attached: bool) {
        // SAFETY: the plugin's view, on its main thread. Null takes the
        // frame back.
        unsafe {
            if attached {
                view.removed();
            }
            view.setFrame(std::ptr::null_mut());
        }
        // Frames refuse everything from here on, and the handlers the
        // editor left registered are released.
        self.editor_loop.end();
        drop(view);
    }

    /// Whether the editor is open.
    pub fn is_editor_open(&self) -> bool {
        self.editor.is_some()
    }

    /// The editor's size, as the plugin reports it now, or the size it last
    /// reported or was given if it cannot say. `None` when no editor is
    /// open.
    pub fn editor_size(&self) -> Option<EditorSize> {
        let open = self.editor.as_ref()?;
        Some(view_size(&open.view).unwrap_or(open.size))
    }

    /// Whether the person may resize the editor's window. False when no
    /// editor is open.
    pub fn can_resize_editor(&self) -> bool {
        self.editor.as_ref().is_some_and(|open| {
            // SAFETY: the plugin's open view, on its main thread.
            unsafe { open.view.canResize() == kResultTrue }
        })
    }

    /// Tells the editor its window is now `size`, and returns the size it
    /// was given.
    ///
    /// Call it when the application has resized the window around the
    /// editor, whether the person did or the application is answering an
    /// [`EditorRequest::Resize`]. A resizable editor is first asked which
    /// size nearest `size` it can take (`checkSizeConstraint`), and given
    /// that; make the window the size returned. An editor of fixed size is
    /// given `size` as it is, and takes only its own.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::EditorNotOpen`] if no editor is open, and
    /// [`Vst3Error::EditorRefused`] if the editor refuses the size; it then
    /// keeps the size it had.
    pub fn set_editor_size(&mut self, size: EditorSize) -> Result<EditorSize, Vst3Error> {
        let open = self.editor.as_ref().ok_or(Vst3Error::EditorNotOpen)?;
        let view = open.view.clone();
        let mut applied = size;
        // SAFETY: the plugin's open view, on its main thread, with
        // rectangles this function owns.
        unsafe {
            if view.canResize() == kResultTrue {
                let mut fitted = rect_of(size);
                if view.checkSizeConstraint(&raw mut fitted) == kResultTrue {
                    applied = rect_size(&raw const fitted).unwrap_or(size);
                }
            }
            let mut rect = rect_of(applied);
            if view.onSize(&raw mut rect) != kResultOk {
                return Err(Vst3Error::EditorRefused("onSize"));
            }
        }
        if let Some(open) = &mut self.editor {
            open.size = applied;
        }
        Ok(applied)
    }

    /// Gives the editor a new content scale, as when its window moved to a
    /// screen of another density. The editor may then ask for a new size,
    /// among the events [`pump`](Self::pump) returns.
    ///
    /// A plugin whose view takes no content scale scales by itself, and
    /// this does nothing.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::InvalidArgument`] for a scale that is not a positive
    /// number, [`Vst3Error::EditorNotOpen`] if no editor is open, and
    /// [`Vst3Error::EditorRefused`] if the editor refuses the scale, as
    /// every editor made with this framework does on macOS.
    pub fn set_editor_scale(&mut self, scale: f64) -> Result<(), Vst3Error> {
        check_scale(scale)?;
        let open = self.editor.as_ref().ok_or(Vst3Error::EditorNotOpen)?;
        let Some(support) = open.scale_support.clone() else {
            return Ok(());
        };
        // SAFETY: the plugin's open view, on its main thread.
        if unsafe { support.setContentScaleFactor(scale as f32) } != kResultOk {
            return Err(Vst3Error::EditorRefused("setContentScaleFactor"));
        }
        Ok(())
    }

    /// Closes the editor: removes it from its window, takes back its frame,
    /// forgets its timers and descriptors, and releases it. The window can
    /// go after that. Does nothing if no editor is open.
    ///
    /// Dropping the plugin closes its editor first.
    pub fn close_editor(&mut self) {
        let Some(open) = self.editor.take() else {
            return;
        };
        self.inbox.set_editor_open(false);
        let OpenEditor {
            view,
            scale_support,
            _frame: frame,
            ..
        } = open;
        drop(scale_support);
        self.abandon(view, true);
        drop(frame);
    }

    /// When the editor's next timer is due, if it has one. Call
    /// [`pump`](Self::pump) then: it fires every timer that is due.
    ///
    /// VST 3 has an editor register timers with its host on Linux, where
    /// it runs from the host's loop; elsewhere an editor keeps its own
    /// timers, and this is `None`.
    pub fn next_editor_timer(&self) -> Option<Instant> {
        self.editor_loop.next_timer()
    }

    /// The file descriptors the editor asked to be told about, on Linux,
    /// where VST 3 has an editor run from its host's loop. Watch them with
    /// the application's own loop, and call
    /// [`editor_fd_ready`](Self::editor_fd_ready) when one is ready. Read
    /// the list again after every call into the plugin: the editor adds
    /// and removes descriptors as it goes. Empty elsewhere.
    pub fn editor_fds(&self) -> Vec<FdWatch> {
        self.editor_loop.fds()
    }

    /// Tells the editor that `fd`, one of [`editor_fds`](Self::editor_fds),
    /// is `ready`. Does nothing for a descriptor the editor does not watch,
    /// or for nothing ready.
    pub fn editor_fd_ready(&mut self, fd: i32, ready: FdInterest) {
        if self.editor.is_some() {
            self.editor_loop.fd_ready(fd, ready);
        }
    }

    // --- The plugin's requests ---------------------------------------------

    /// How many blocks have come out silent because the plugin failed,
    /// since it was created.
    pub fn failed_blocks(&self) -> u64 {
        self.shared.failed_blocks.load(Ordering::Relaxed)
    }

    /// How many events and automation points have been dropped for want of
    /// room, since the plugin was created: on the way to the plugin beyond
    /// [`EVENTS_PER_BLOCK`](super::EVENTS_PER_BLOCK) or
    /// [`POINTS_PER_BLOCK`](super::POINTS_PER_BLOCK) in one call, on the way
    /// back beyond what the lists hold, waiting for the next block beyond
    /// the 1,024 that can wait, and for a main thread that had stopped
    /// calling [`pump`](Self::pump).
    ///
    /// Anything but zero means something was not heard or not seen.
    pub fn lost_events(&self) -> u64 {
        self.shared.lost_events.load(Ordering::Relaxed)
    }

    /// How many events have not been sent because the plugin has no way to
    /// receive them, since it was created: notes for a plugin with no event
    /// input, values for parameters it does not have, controllers it maps
    /// to nothing, note expression for a note with no id, modulation, and
    /// numbers that are not numbers.
    pub fn undeliverable_events(&self) -> u64 {
        self.shared.undeliverable_events.load(Ordering::Relaxed)
    }

    /// Passes on what the plugin reported since the last call, and returns
    /// what there is to know.
    ///
    /// - An edit the controller made is sent on to the processor, as VST 3
    ///   has a host do, and returned as [`HostEvent::ParamValue`] between
    ///   [`HostEvent::GestureBegin`] and [`HostEvent::GestureEnd`].
    /// - A value the processor reported is told to the controller and
    ///   returned as [`HostEvent::ParamValue`].
    /// - Values the audio half sent the processor are told to the
    ///   controller, so that it shows what is heard.
    /// - A restart request that changes parameters has the list read again;
    ///   one that needs a new activation is returned.
    /// - While the plugin is active, its tail is read again, for
    ///   [`Vst3Processor::tail`] to report.
    /// - The editor's timers that are due fire, first, so that what they do
    ///   is passed on in the same call, and what the editor asked for is
    ///   returned as [`HostEvent::Editor`].
    ///
    /// Call it regularly: a few times a second is enough for most plugins,
    /// and a user interface wants it on every frame. While the editor has
    /// timers, call it again by [`next_editor_timer`](Self::next_editor_timer).
    pub fn pump(&mut self) -> Vec<HostEvent> {
        if self.editor.is_some() {
            self.editor_loop
                .fire_timers(Instant::now(), &mut self.due_timers);
        }
        self.pass_on();
        self.restart(self.inbox.take_restart());
        let requests = self.inbox.take_requests();
        if requests & request::DIRTY != 0 {
            self.waiting.push(HostEvent::StateDirty);
        }
        if requests & request::OPEN_EDITOR != 0 {
            self.waiting.push(HostEvent::EditorRequested);
        }
        if requests & request::OTHER_VIEW != 0 {
            self.remark(
                "the plugin asked for a view other than its editor, which this host does not open"
                    .to_owned(),
            );
        }
        if let Some(size) = self.editor_loop.take_resize() {
            self.waiting
                .push(HostEvent::Editor(EditorRequest::Resize(size)));
        }
        let wrong = self.inbox.take_off_thread();
        if wrong > 0 {
            self.remark(format!(
                "the plugin called a main-thread function of the host from another thread \
                 {wrong} time(s)"
            ));
        }
        let refused = self.editor_loop.take_off_thread();
        if refused > 0 {
            self.remark(format!(
                "the plugin called its editor's frame or run loop from another thread \
                 {refused} time(s); the calls were refused"
            ));
        }
        for failure in ProcessFailure::from_bits(self.shared.take_failures()) {
            self.waiting.push(HostEvent::ProcessFailed(failure));
        }
        if self.shared.active() {
            self.tail();
        }
        std::mem::take(&mut self.waiting)
    }

    /// The part of [`pump`](Self::pump) that moves parameter values: what
    /// the processor was sent and what it reported goes to the controller,
    /// and the edits the controller made go to the processor. The events
    /// for the caller wait for the next `pump`.
    fn pass_on(&mut self) {
        // What the processor was sent, and what it reported.
        if let Some(mut link) = self.link.take() {
            self.drain(&mut link);
            self.link = Some(link);
        }

        // What the controller did.
        for edit in self.inbox.take_edits() {
            match edit {
                Edit::Begin(id) => self
                    .waiting
                    .push(HostEvent::GestureBegin { param: ParamId(id) }),
                Edit::Perform(id, value) => {
                    let value = unit(value);
                    if self.slot(ParamId(id)).is_none() {
                        self.remark(format!(
                            "the plugin's controller edited parameter {id}, which is not in its \
                             list; the edit was dropped"
                        ));
                        continue;
                    }
                    if self.send(id, value).is_err() {
                        self.shared.lost_events.fetch_add(1, Ordering::Relaxed);
                    }
                    self.waiting.push(HostEvent::ParamValue {
                        param: ParamId(id),
                        value,
                    });
                }
                Edit::End(id) => self
                    .waiting
                    .push(HostEvent::GestureEnd { param: ParamId(id) }),
            }
        }
        let lost = self.inbox.take_lost_edits();
        if lost > 0 {
            self.remark(format!(
                "{lost} edit(s) from the plugin's controller were dropped: it edits faster than \
                 it is read"
            ));
        }
    }

    /// Acts on a plugin's `restartComponent` flags.
    fn restart(&mut self, flags: u32) {
        if flags == 0 {
            return;
        }
        let flags = flags as i32;
        let has = |flag: vst::RestartFlags| flags & flag as i32 != 0;
        let values = has(RestartFlags_::kParamValuesChanged);
        let info =
            has(RestartFlags_::kParamTitlesChanged) || has(RestartFlags_::kParamIDMappingChanged);
        if values || info {
            if info {
                self.read_params();
            }
            self.waiting
                .push(HostEvent::ParamsRescanned { values, info });
        }
        if has(RestartFlags_::kIoTitlesChanged) || has(RestartFlags_::kIoChanged) {
            if self.shared.active() {
                self.buses_stale = true;
            } else {
                self.read_buses();
            }
        }
        let needs_restart = (RestartFlags_::kReloadComponent
            | RestartFlags_::kIoChanged
            | RestartFlags_::kLatencyChanged
            | RestartFlags_::kMidiCCAssignmentChanged) as i32;
        if flags & needs_restart != 0 {
            self.waiting.push(HostEvent::RestartRequested {
                flags: flags & needs_restart,
            });
        }
    }
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        // The editor goes first, here on the main thread, while the
        // controller that made it is still initialised.
        self.close_editor();
        // Closes the plugin now if its audio half is gone, and leaves that
        // to the audio half if it is not.
        self.shared.main_handle_dropped();
    }
}

impl std::fmt::Debug for Vst3Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Plugin")
            .field("class", &self.class.id)
            .field("name", &self.class.name)
            .field("active", &self.is_active())
            .field("params", &self.params.len())
            .finish_non_exhaustive()
    }
}

/// Refuses a content scale that is not a positive number, in `f32` as VST 3
/// passes it.
fn check_scale(scale: f64) -> Result<(), Vst3Error> {
    let narrow = scale as f32;
    if scale.is_finite() && scale > 0.0 && narrow.is_finite() && narrow > 0.0 {
        Ok(())
    } else {
        Err(Vst3Error::InvalidArgument(
            "a content scale has to be a positive number",
        ))
    }
}

/// The VST 3 platform type of a parent window, and the handle `attached`
/// is given for it.
fn platform_of(parent: ParentWindow) -> Option<(FIDString, *mut c_void)> {
    let (kind, handle) = match parent {
        ParentWindow::Win32 { hwnd } => (sb::kPlatformTypeHWND, hwnd.get()),
        ParentWindow::Cocoa { ns_view } => (sb::kPlatformTypeNSView, ns_view.get()),
        // VST 3 passes an X11 window's id in place of the pointer.
        ParentWindow::X11 { window } => (sb::kPlatformTypeX11EmbedWindowID, window.get() as usize),
        _ => return None,
    };
    // The handle is the application's: a number to the plugin, or the
    // address of an `NSView` it calls, which is why it keeps its
    // provenance.
    Some((kind, std::ptr::with_exposed_provenance_mut(handle)))
}

/// The size a view reports, or `None` if it does not say or reports a
/// rectangle inside out.
fn view_size(view: &ComPtr<IPlugView>) -> Option<EditorSize> {
    let mut rect = ViewRect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: the plugin's view, on its main thread, with a rectangle this
    // function owns.
    if unsafe { view.getSize(&raw mut rect) } != kResultOk {
        return None;
    }
    // SAFETY: as above.
    unsafe { rect_size(&raw const rect) }
}

/// Makes and initialises the separate controller a component names, or
/// returns `None`, with a remark, if it cannot be had.
fn make_controller(
    module: &Vst3Module,
    component: &ComPtr<IComponent>,
    host: &vst3::ComWrapper<HostApplication>,
    remarks: &mut Vec<String>,
) -> Option<ComPtr<IEditController>> {
    let mut class: TUID = [0; 16];
    // SAFETY: the plugin's initialised component, on its main thread, with
    // 16 bytes this function owns.
    let named = unsafe { component.getControllerClassId(&raw mut class) };
    let class = ClassId::from_tuid(&class);
    if named != kResultOk || class.is_zero() {
        remarks.push("the plugin has no controller, and so no parameters".to_owned());
        return None;
    }
    let Some(controller) = module.create_instance::<IEditController>(class) else {
        remarks.push(format!(
            "the plugin's controller, class {class}, could not be made; it has no parameters"
        ));
        return None;
    };
    // SAFETY: a new controller, initialised once, first, on its main
    // thread, with a host context that outlives it.
    if unsafe { controller.initialize(context_ptr(host)) } != kResultOk {
        remarks.push(format!(
            "the plugin's controller, class {class}, refused to initialise; it has no parameters"
        ));
        return None;
    }
    Some(controller)
}
