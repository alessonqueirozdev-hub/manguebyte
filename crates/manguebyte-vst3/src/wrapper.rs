//! A [`Plugin`] as a VST 3 component.
//!
//! One object implements the processor (`IComponent`, `IAudioProcessor`)
//! and the controller (`IEditController`) together. VST 3 allows the two
//! to be separate objects that only exchange messages, which matters when
//! they run on different machines; a plugin whose parameters are atomics
//! shared between threads has no use for that, and keeping one object means
//! there is one copy of every value.
//!
//! # Threads
//!
//! VST 3 says which thread calls what, and this file leans on those rules
//! instead of on locks:
//!
//! - `process` is called on the audio thread.
//! - `setupProcessing` and `setActive` are called on the main thread, and
//!   only while the component is not processing.
//! - Everything else is called on the main thread, possibly while `process`
//!   is running.
//!
//! So the audio-side state ([`Audio`]) is touched by one thread at a time
//! and lives in an `UnsafeCell`. Everything the main thread needs while
//! audio runs goes through the parameters, which are atomics.
//!
//! # A value set on the controller is not a value for the processor
//!
//! A host tells a plugin about a parameter change twice. The processor gets
//! it in `process`, as a point with a frame number. The controller gets it
//! through `setParamNormalized`, on the main thread, some time before or
//! after, and only so that an editor can show it.
//!
//! Being one object, it is tempting to store the controller's copy straight
//! into the parameter. That breaks automation: the main thread's value is a
//! little early or a little late, the audio thread finds the parameter
//! changed, and the plugin jumps to it at the start of a block, off the
//! curve the host is sending, until the next point puts it back.
//!
//! So while the host is delivering blocks, `setParamNormalized` only
//! updates what the controller reports ([`Controls::shown`]) and the
//! points in `process` are the one source of the parameter's value. While
//! it is not delivering blocks there are no points to wait for, and the
//! value is stored at once so that it is not lost; the plugin hears about
//! it as an event on the first frame of its next block.
//!
//! # Edits made in the plugin's editor
//!
//! The same rule holds for the plugin's own editor. Its edits reach the
//! host through the `IComponentHandler` the host gave in
//! `setComponentHandler`: `beginEdit`, then `performEdit` with each value
//! the control set, then `endEdit`, on the main thread (see
//! [`crate::view`]). The value also becomes what the controller reports.
//! While blocks run it reaches the processor as the host's own point in
//! `process`, like any other automation; while they do not, it is stored
//! at once, as `setParamNormalized` stores it. No edit is ever inferred by
//! comparing values, which would report the host's automation and restored
//! state back to it as a person's edits.

// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast that is needed on one platform is redundant on the
// other.
#![allow(clippy::unnecessary_cast)]

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use manguebyte_core::{AtomicF64, AudioBuffer, NoDenormals, ParamId};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Transport};
use manguebyte_midi::MidiMessage;
use manguebyte_params::{Param, ParamSet};
use manguebyte_plugin::{
    ParamWatcher, Plugin, PluginKind, apply_param_events, load_state, save_state,
};
use vst3::Steinberg::Vst::{self as vst, BusDirections_, BusTypes_, MediaTypes_};
use vst3::Steinberg::{
    self as sb, FUnknown, IBStream, IBStreamTrait, IPluginBaseTrait, TBool, TUID, kInvalidArgument,
    kNotImplemented, kResultFalse, kResultOk, tresult,
};
use vst3::{Class, ComRef};

use crate::handler::Handler;
use crate::text::{copy_wide, param_id, read_wide};
use crate::view::Editors;

/// Most events delivered to the plugin in one block. Extra events are
/// dropped rather than allocated for.
const EVENTS_PER_BLOCK: usize = 4_096;

/// Most controller moves and notes kept from blocks that carried no audio
/// until a block that does.
const HELD_EVENTS: usize = 256;

/// Largest state a host may hand back, so a corrupt stream cannot make the
/// plugin allocate without bound.
const MAX_STATE_BYTES: usize = 64 << 20;

/// VST 3 does not deliver MIDI controllers as events. A plugin that wants
/// them declares one hidden parameter per controller and channel, and the
/// host moves those parameters. These are their ids: 130 controllers (the
/// 128 of MIDI, channel pressure and pitch bend) for each of 16 channels.
const MIDI_PARAM_BASE: u32 = 0x7F00_0000;
const MIDI_CONTROLLERS: u32 = vst::ControllerNumbers_::kCountCtrlNumber as u32;
const MIDI_CHANNELS: u32 = 16;
const MIDI_PARAM_COUNT: u32 = MIDI_CONTROLLERS * MIDI_CHANNELS;

/// The unit the hidden controller parameters are filed under, so that a
/// host which lists parameters by unit keeps two thousand of them apart
/// from the plugin's own.
const MIDI_UNIT: vst::UnitID = 1;

/// Runs `work`, turning a panic into `fallback`.
///
/// A panic must never cross into the host: unwinding through a foreign
/// function is not defined, and aborting would take the user's session
/// with it.
pub(crate) fn guard<R>(fallback: R, work: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(work)).unwrap_or(fallback)
}

/// The per-note expressions offered to hosts, with their VST 3 ids and
/// titles.
const EXPRESSIONS: [(u32, &str, &str); 4] = [
    (
        vst::NoteExpressionTypeIDs_::kVolumeTypeID as u32,
        "Volume",
        "Vol",
    ),
    (vst::NoteExpressionTypeIDs_::kPanTypeID as u32, "Pan", "Pan"),
    (
        vst::NoteExpressionTypeIDs_::kTuningTypeID as u32,
        "Tuning",
        "Tune",
    ),
    (
        vst::NoteExpressionTypeIDs_::kBrightnessTypeID as u32,
        "Brightness",
        "Brit",
    ),
];

/// Converts a VST 3 note expression, which is always a position in `0..=1`,
/// to the unit documented on [`Expression`].
fn expression_from_vst(type_id: u32, value: f64) -> Option<(Expression, f64)> {
    use vst::NoteExpressionTypeIDs_ as ids;
    Some(match type_id {
        // A quarter of the way up is unity gain; the top is +12 dB.
        id if id == ids::kVolumeTypeID as u32 => (Expression::Volume, 4.0 * value),
        id if id == ids::kPanTypeID as u32 => (Expression::Pan, 2.0 * value - 1.0),
        // The full range is ten octaves either way.
        id if id == ids::kTuningTypeID as u32 => (Expression::Tuning, 240.0 * (value - 0.5)),
        id if id == ids::kVibratoTypeID as u32 => (Expression::Vibrato, value),
        id if id == ids::kBrightnessTypeID as u32 => (Expression::Brightness, value),
        _ => return None,
    })
}

/// The position in `0..=1` at which a VST 3 note expression has the value
/// `plain`, in the unit documented on [`Expression`]. The inverse of
/// [`expression_from_vst`].
fn expression_to_vst(type_id: u32, plain: f64) -> Option<f64> {
    use vst::NoteExpressionTypeIDs_ as ids;
    let position = match type_id {
        id if id == ids::kVolumeTypeID as u32 => plain / 4.0,
        id if id == ids::kPanTypeID as u32 => (plain + 1.0) / 2.0,
        id if id == ids::kTuningTypeID as u32 => plain / 240.0 + 0.5,
        id if id == ids::kVibratoTypeID as u32 || id == ids::kBrightnessTypeID as u32 => plain,
        _ => return None,
    };
    Some(position.clamp(0.0, 1.0))
}

/// Whether the plugin has an event bus and the controllers that go with it.
fn takes_notes<P: Plugin>() -> bool {
    matches!(
        P::INFO.kind,
        PluginKind::Instrument | PluginKind::NoteEffect
    )
}

/// The speaker arrangement for a bus of `channels` channels.
fn arrangement(channels: usize) -> vst::SpeakerArrangement {
    match channels {
        0 => vst::SpeakerArr::kEmpty,
        1 => vst::SpeakerArr::kMono,
        2 => vst::SpeakerArr::kStereo,
        // No named layout: one speaker bit per channel.
        more => (1u64 << more.min(63)) - 1,
    }
}

/// The channel counts of the audio buses in one direction, main bus first.
fn audio_buses<P: Plugin>(is_input: bool) -> ([usize; 2], usize) {
    let layout = P::LAYOUT;
    let candidates = if is_input {
        [layout.inputs, layout.sidechain]
    } else {
        [layout.outputs, 0]
    };
    let mut present = [0; 2];
    let mut count = 0;
    for channels in candidates {
        if channels > 0 {
            present[count] = channels;
            count += 1;
        }
    }
    (present, count)
}

/// The state used while processing.
struct Audio<P> {
    plugin: P,
    sample_rate: f64,
    max_block: usize,
    /// Whether the plugin has been prepared for the current setup.
    prepared: bool,
    buffer: AudioBuffer<f32>,
    sidechain: AudioBuffer<f32>,
    events: EventBuffer,
    /// Controller moves and notes that arrived in a block with no audio,
    /// for the next block that has some.
    held: EventBuffer,
    /// Parameter values as the plugin last saw them.
    watcher: ParamWatcher,
}

/// The parameters as the controller lists and reports them, shared with the
/// plugin's open editors, which may outlive the component.
pub(crate) struct Controls {
    params: Arc<dyn ParamSet>,
    /// Every parameter's own id and its VST 3 id, in the order the host
    /// indexes them.
    ids: Vec<(ParamId, u32)>,
    /// The normalized value of each parameter as the controller reports
    /// it, in the order of `ids`. See the module documentation.
    shown: Vec<AtomicF64>,
    /// Whether the host is delivering blocks, so that parameter changes
    /// arrive in `process`.
    running: AtomicBool,
}

impl Controls {
    /// Where the plugin parameter with this VST 3 id is in the host's list.
    pub(crate) fn index_of(&self, id: u32) -> Option<usize> {
        self.ids.iter().position(|&(_, vst)| vst == id)
    }

    /// Where the plugin parameter `param` is in the host's list.
    pub(crate) fn index_of_param(&self, param: ParamId) -> Option<usize> {
        self.ids.iter().position(|&(own, _)| own == param)
    }

    /// The VST 3 id of the parameter at `index` in the host's list.
    pub(crate) fn vst_id(&self, index: usize) -> Option<u32> {
        self.ids.get(index).map(|&(_, id)| id)
    }

    /// The plugin parameter with this VST 3 id.
    fn param(&self, id: u32) -> Option<&dyn Param> {
        self.params.find_param(self.ids[self.index_of(id)?].0)
    }

    /// Makes the controller report the parameters' own values again, after
    /// something other than the host's controller calls changed them.
    pub(crate) fn show_current_values(&self) {
        let mut shown = self.shown.iter();
        self.params.visit(&mut |param| {
            if let Some(shown) = shown.next() {
                shown.store(param.normalized(), Ordering::Relaxed);
            }
        });
    }

    /// The controller's value of the parameter at `index` is now
    /// `normalized`, clamped to `0..=1`: from the host's
    /// `setParamNormalized`, or from an edit in the plugin's editor.
    ///
    /// While blocks run, the same value arrives in `process` as the host's
    /// point, on its frame, and that is the one the plugin follows; while
    /// they do not, it is stored into the parameter at once (see the module
    /// documentation).
    pub(crate) fn set_normalized(&self, index: usize, normalized: f64) {
        let (Some(shown), Some(&(own, _))) = (self.shown.get(index), self.ids.get(index)) else {
            return;
        };
        shown.store(normalized.clamp(0.0, 1.0), Ordering::Relaxed);
        if !self.running.load(Ordering::Relaxed) {
            if let Some(param) = self.params.find_param(own) {
                param.set_normalized(normalized);
            }
        }
    }
}

/// One plugin as the host sees it.
pub(crate) struct Component<P: Plugin> {
    params: Arc<P::Params>,
    controls: Arc<Controls>,
    /// Latency reported by the plugin when it was last activated.
    latency: AtomicU32,
    /// The unit the host last selected. Nothing here depends on it; it is
    /// kept so that the host is told the same thing back.
    selected_unit: AtomicI32,
    /// See the module documentation for who may touch this and when.
    audio: UnsafeCell<Audio<P>>,
    /// The host's component handler. Main thread only, like everything
    /// below: the audio thread never touches these.
    handler: Rc<Handler>,
    /// How to make views of the plugin's editor, if it has one.
    editors: Option<Rc<Editors>>,
}

impl<P: Plugin> Class for Component<P> {
    type Interfaces = (
        vst::IComponent,
        vst::IAudioProcessor,
        vst::IEditController,
        vst::IMidiMapping,
        vst::INoteExpressionController,
        vst::IProcessContextRequirements,
        vst::IUnitInfo,
    );
}

impl<P: Plugin> Component<P> {
    /// Creates the plugin, or returns `None` if it cannot be a VST 3
    /// plugin: its parameters do not validate, or two of them end up with
    /// the same VST 3 id.
    pub(crate) fn new() -> Option<Self> {
        let plugin = P::new();
        let params = plugin.params();
        params.validate().ok()?;
        // Once, right after the parameters, as the editor contract says.
        let spec = plugin.editor();

        let mut ids = Vec::with_capacity(params.len());
        params.visit(&mut |param| ids.push((param.param_id(), param_id(param.param_id()))));
        let mut seen: Vec<u32> = ids.iter().map(|&(_, id)| id).collect();
        seen.sort_unstable();
        let clash = seen.windows(2).any(|pair| pair[0] == pair[1]);
        let in_midi_range = seen
            .iter()
            .any(|&id| (MIDI_PARAM_BASE..MIDI_PARAM_BASE + MIDI_PARAM_COUNT).contains(&id));
        if clash || in_midi_range {
            return None;
        }

        let mut shown = Vec::with_capacity(params.len());
        params.visit(&mut |param| shown.push(AtomicF64::new(param.normalized())));

        let shared: Arc<dyn ParamSet> = params.clone();
        let controls = Arc::new(Controls {
            params: shared,
            ids,
            shown,
            running: AtomicBool::new(false),
        });
        let handler = Rc::new(Handler::new());
        let editors =
            spec.map(|spec| Editors::new(spec, Arc::clone(&controls), Rc::clone(&handler)));

        Some(Self {
            controls,
            latency: AtomicU32::new(0),
            selected_unit: AtomicI32::new(vst::kRootUnitId),
            audio: UnsafeCell::new(Audio {
                watcher: ParamWatcher::new(&*params),
                plugin,
                sample_rate: 44_100.0,
                max_block: 0,
                prepared: false,
                buffer: AudioBuffer::new(P::LAYOUT.outputs, 0),
                sidechain: AudioBuffer::new(P::LAYOUT.sidechain, 0),
                events: EventBuffer::new(),
                held: EventBuffer::new(),
            }),
            params,
            handler,
            editors,
        })
    }

    /// Where the plugin parameter with this VST 3 id is in the host's list.
    fn index_of(&self, id: u32) -> Option<usize> {
        self.controls.index_of(id)
    }

    /// The plugin parameter with this VST 3 id.
    fn param(&self, id: u32) -> Option<&dyn Param> {
        self.controls.param(id)
    }

    /// Makes the controller report the parameters' own values again, after
    /// something other than the host's controller calls changed them.
    fn show_current_values(&self) {
        self.controls.show_current_values();
    }

    /// Closes every editor of the plugin and lets go of the host's handler,
    /// for `terminate` and for when the component goes away. Editors are
    /// closed first, so that the edits they leave open are ended on the
    /// handler before it is released.
    fn shut_down(&self) {
        if let Some(editors) = &self.editors {
            editors.shutdown();
        }
        self.handler.set(None);
    }

    /// How many hidden parameters stand in for MIDI controllers.
    fn midi_param_count() -> u32 {
        if takes_notes::<P>() {
            MIDI_PARAM_COUNT
        } else {
            0
        }
    }

    /// The MIDI message a move of a hidden controller parameter stands for.
    fn midi_from_param(id: u32, value: f64) -> Option<MidiMessage> {
        let index = id.checked_sub(MIDI_PARAM_BASE)?;
        if index >= Self::midi_param_count() {
            return None;
        }
        let channel = (index / MIDI_CONTROLLERS) as u8;
        let controller = index % MIDI_CONTROLLERS;
        let value = value.clamp(0.0, 1.0);
        Some(match controller {
            c if c == vst::ControllerNumbers_::kPitchBend as u32 => MidiMessage::PitchBend {
                channel,
                value: ((value * 16_383.0).round() as i32 - 8_192) as i16,
            },
            c if c == vst::ControllerNumbers_::kAfterTouch as u32 => MidiMessage::ChannelPressure {
                channel,
                pressure: (value * 127.0).round() as u8,
            },
            c => MidiMessage::ControlChange {
                channel,
                controller: c as u8,
                value: (value * 127.0).round() as u8,
            },
        })
    }

    /// Fills `audio.events` for the coming block from what the host sent.
    ///
    /// A block with no audio (`has_audio` false) is how a host delivers
    /// changes while it is not playing. The plugin is not run for it, so
    /// there is nobody to hand events to: parameter values are stored and
    /// left for the next block with audio to announce, and everything else
    /// is held until then. `audio.events` is left empty.
    ///
    /// # Safety
    ///
    /// The pointers in `data` must be valid for the duration of the call,
    /// or null.
    unsafe fn gather_events(&self, audio: &mut Audio<P>, data: &vst::ProcessData, has_audio: bool) {
        use vst::{IEventListTrait, IParamValueQueueTrait, IParameterChangesTrait};

        let params: &dyn ParamSet = &*self.params;
        audio.events.clear();
        let mut announced = 0;
        if has_audio {
            // What blocks without audio left behind goes first.
            for &event in &audio.held {
                let _ = audio.events.push(event);
            }
            audio.held.clear();
            // Then changes the plugin has not seen yet, on frame 0: a
            // restored state, or a value set while no blocks were running.
            audio.watcher.emit_changes(params, &mut audio.events);
            announced = audio.events.len();
        }

        // SAFETY: the host's object, valid for this call, or null.
        if let Some(changes) = unsafe { ComRef::from_raw(data.inputParameterChanges) } {
            // SAFETY: calls on the host's own object, with indices below
            // the counts it just reported and out-pointers to our locals.
            unsafe {
                for index in 0..changes.getParameterCount() {
                    let Some(queue) = ComRef::from_raw(changes.getParameterData(index)) else {
                        continue;
                    };
                    let id = queue.getParameterId();
                    for point in 0..queue.getPointCount() {
                        let (mut offset, mut value) = (0i32, 0f64);
                        if queue.getPoint(point, &mut offset, &mut value) != kResultOk {
                            continue;
                        }
                        let offset = offset.max(0) as u32;
                        let kind = if let Some(param) = self.param(id) {
                            EventKind::ParamValue {
                                param: param.param_id(),
                                value: param.normalized_to_plain(value),
                            }
                        } else if let Some(message) = Self::midi_from_param(id, value) {
                            Event::from_midi(offset, message).kind
                        } else {
                            continue;
                        };
                        let _ = audio.events.push(Event { offset, kind });
                    }
                }
            }
        }

        // SAFETY: the host's object, valid for this call, or null.
        if let Some(list) = unsafe { ComRef::from_raw(data.inputEvents) } {
            // SAFETY: as above. A zeroed event is a valid value to
            // overwrite: every field is a number.
            unsafe {
                for index in 0..list.getEventCount() {
                    let mut event: vst::Event = std::mem::zeroed();
                    if list.getEvent(index, &mut event) == kResultOk {
                        convert_event(&event, &mut audio.events);
                    }
                }
            }
        }

        if has_audio {
            // Only the host's values are stored. The announced ones were
            // read from the parameters, and the main thread may have stored
            // a newer value since (a state load, or an editor's edit while
            // no blocks ran); writing the old one back would undo it.
            let from_host = &audio.events.as_slice()[announced..];
            audio.watcher.apply_and_remember(params, from_host);
            return;
        }
        apply_param_events(params, &audio.events);
        // The values are stored, and the watcher has not been told, so it
        // announces them on the next block with audio. A controller move
        // or a note has no stored value to be found again, so it is kept.
        for event in &audio.events {
            if !matches!(event.kind, EventKind::ParamValue { .. }) {
                let kind = event.kind;
                let _ = audio.held.push(Event { offset: 0, kind });
            }
        }
        audio.events.clear();
    }
}

/// Turns one VST 3 event into ours and queues it.
///
/// # Safety
///
/// `event` must have been filled in by a host, so that the member of its
/// union named by its type is the one that was written.
unsafe fn convert_event(event: &vst::Event, events: &mut EventBuffer) {
    use vst::Event_::EventTypes_ as types;

    let offset = event.sampleOffset.max(0) as u32;
    let note_ref = |channel: i16, pitch: i16, id: i32| NoteRef {
        channel: (channel.clamp(0, 15)) as u8,
        key: (pitch.clamp(0, 127)) as u8,
        // -1 means the host gave the note no identifier.
        id: (id != -1).then_some(id as u32),
    };
    let mut push = |kind: EventKind| {
        // A full buffer drops the event; allocating here is not an option.
        let _ = events.push(Event { offset, kind });
    };

    let kind = u32::from(event.r#type);
    if kind == types::kNoteOnEvent as u32 {
        // SAFETY: the type says the union holds a note-on.
        let on = unsafe { event.__field0.noteOn };
        let note = note_ref(on.channel, on.pitch, on.noteId);
        push(EventKind::NoteOn {
            note,
            velocity: on.velocity,
        });
        if on.tuning != 0.0 {
            // Detuning arrives in cents with the note itself.
            push(EventKind::NoteExpression {
                note,
                expression: Expression::Tuning,
                value: f64::from(on.tuning) / 100.0,
            });
        }
    } else if kind == types::kNoteOffEvent as u32 {
        // SAFETY: the type says the union holds a note-off.
        let off = unsafe { event.__field0.noteOff };
        push(EventKind::NoteOff {
            note: note_ref(off.channel, off.pitch, off.noteId),
            velocity: off.velocity,
        });
    } else if kind == types::kPolyPressureEvent as u32 {
        // SAFETY: the type says the union holds a pressure event.
        let pressure = unsafe { event.__field0.polyPressure };
        push(EventKind::NoteExpression {
            note: note_ref(pressure.channel, pressure.pitch, pressure.noteId),
            expression: Expression::Pressure,
            value: f64::from(pressure.pressure),
        });
    } else if kind == types::kNoteExpressionValueEvent as u32 {
        // SAFETY: the type says the union holds a note expression.
        let shaped = unsafe { event.__field0.noteExpressionValue };
        // An expression names its note by identifier alone.
        if shaped.noteId == -1 {
            return;
        }
        if let Some((expression, value)) = expression_from_vst(shaped.typeId, shaped.value) {
            push(EventKind::NoteExpression {
                note: NoteRef::with_id(0, 0, shaped.noteId as u32),
                expression,
                value,
            });
        }
    }
}

fn convert_transport(context: &vst::ProcessContext) -> Transport {
    use vst::ProcessContext_::StatesAndFlags_ as flags;
    let has = |flag: u32| context.state & flag != 0;
    Transport {
        playing: has(flags::kPlaying as u32),
        recording: has(flags::kRecording as u32),
        tempo_bpm: has(flags::kTempoValid as u32).then_some(context.tempo),
        time_signature: has(flags::kTimeSigValid as u32).then(|| {
            (
                context.timeSigNumerator.clamp(0, 65_535) as u16,
                context.timeSigDenominator.clamp(0, 65_535) as u16,
            )
        }),
        position_samples: Some(context.projectTimeSamples),
        position_beats: has(flags::kProjectTimeMusicValid as u32)
            .then_some(context.projectTimeMusic),
        bar_start_beats: has(flags::kBarPositionValid as u32).then_some(context.barPositionMusic),
        loop_beats: (has(flags::kCycleActive as u32) && has(flags::kCycleValid as u32))
            .then_some((context.cycleStartMusic, context.cycleEndMusic)),
    }
}

/// The host's audio buses as a slice.
///
/// # Safety
///
/// `first` must point to `count` valid buses, or be null.
unsafe fn buses<'a>(
    first: *mut vst::AudioBusBuffers,
    count: i32,
) -> &'a mut [vst::AudioBusBuffers] {
    if first.is_null() || count <= 0 {
        return &mut [];
    }
    // SAFETY: guaranteed by this function's contract.
    unsafe { std::slice::from_raw_parts_mut(first, count as usize) }
}

/// Copies up to `channels` channels of a host bus into `buffer`.
///
/// # Safety
///
/// `bus` must hold valid 32-bit channel pointers, each with at least
/// `buffer.num_frames()` frames, or a null channel array.
unsafe fn read_bus(bus: &vst::AudioBusBuffers, buffer: &mut AudioBuffer<f32>, channels: usize) {
    // SAFETY: the plugin only accepts 32-bit processing, so this is the
    // member of the union the host filled in.
    let pointers = unsafe { bus.__field0.channelBuffers32 };
    if pointers.is_null() {
        return;
    }
    let frames = buffer.num_frames();
    let count = channels
        .min(bus.numChannels.max(0) as usize)
        .min(buffer.num_channels());
    for index in 0..count {
        // SAFETY: `index` is below the bus's channel count.
        let channel = unsafe { *pointers.add(index) };
        if channel.is_null() {
            continue;
        }
        // SAFETY: the host guarantees `frames` readable samples per channel.
        let samples = unsafe { std::slice::from_raw_parts(channel, frames) };
        buffer.channel_mut(index).copy_from_slice(samples);
    }
}

/// Fills a host bus from `source`, or with silence where `source` has no
/// channel for it.
///
/// # Safety
///
/// `bus` must hold valid 32-bit channel pointers, each with at least
/// `frames` writable frames, or a null channel array.
unsafe fn write_bus(
    bus: &mut vst::AudioBusBuffers,
    source: Option<&AudioBuffer<f32>>,
    frames: usize,
) {
    // SAFETY: as in `read_bus`.
    let pointers = unsafe { bus.__field0.channelBuffers32 };
    let channels = bus.numChannels.max(0) as usize;
    let mut silent = 0u64;
    if !pointers.is_null() {
        for index in 0..channels {
            // SAFETY: `index` is below the bus's channel count.
            let channel = unsafe { *pointers.add(index) };
            if channel.is_null() {
                continue;
            }
            // SAFETY: the host guarantees `frames` writable samples per
            // channel, and no reference to the host's input is alive here.
            let samples = unsafe { std::slice::from_raw_parts_mut(channel, frames) };
            match source.filter(|buffer| index < buffer.num_channels()) {
                Some(buffer) => samples.copy_from_slice(buffer.channel(index)),
                None => {
                    samples.fill(0.0);
                    silent |= 1 << index.min(63);
                }
            }
        }
    }
    bus.silenceFlags = silent;
}

impl<P: Plugin> IPluginBaseTrait for Component<P> {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        if let Some(editors) = &self.editors {
            // Views may be made again after a `terminate`.
            editors.reopen();
        }
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        guard(kResultFalse, || {
            self.shut_down();
            kResultOk
        })
    }
}

impl<P: Plugin> vst::IComponentTrait for Component<P> {
    unsafe fn getControllerClassId(&self, _class_id: *mut TUID) -> tresult {
        // The controller is this same object.
        kNotImplemented
    }

    unsafe fn setIoMode(&self, _mode: vst::IoMode) -> tresult {
        kNotImplemented
    }

    unsafe fn getBusCount(&self, media: vst::MediaType, direction: vst::BusDirection) -> i32 {
        let is_input = direction == BusDirections_::kInput as vst::BusDirection;
        if media == MediaTypes_::kAudio as vst::MediaType {
            audio_buses::<P>(is_input).1 as i32
        } else if media == MediaTypes_::kEvent as vst::MediaType {
            i32::from(is_input && takes_notes::<P>())
        } else {
            0
        }
    }

    unsafe fn getBusInfo(
        &self,
        media: vst::MediaType,
        direction: vst::BusDirection,
        index: i32,
        bus: *mut vst::BusInfo,
    ) -> tresult {
        // SAFETY: a struct the host owns, or null.
        let Some(bus) = (unsafe { bus.as_mut() }) else {
            return kInvalidArgument;
        };
        let is_input = direction == BusDirections_::kInput as vst::BusDirection;
        let default_active = vst::BusInfo_::BusFlags_::kDefaultActive as u32;

        if media == MediaTypes_::kEvent as vst::MediaType {
            if index != 0 || !is_input || !takes_notes::<P>() {
                return kInvalidArgument;
            }
            bus.mediaType = media;
            bus.direction = direction;
            bus.channelCount = MIDI_CHANNELS as i32;
            copy_wide("Notes", &mut bus.name);
            bus.busType = BusTypes_::kMain as vst::BusType;
            bus.flags = default_active;
            return kResultOk;
        }
        if media != MediaTypes_::kAudio as vst::MediaType {
            return kInvalidArgument;
        }

        let (channels, count) = audio_buses::<P>(is_input);
        if index < 0 || index as usize >= count {
            return kInvalidArgument;
        }
        // The main bus is the first one, except that an instrument's only
        // input is its sidechain.
        let is_main = index == 0 && !(is_input && P::LAYOUT.inputs == 0);
        bus.mediaType = media;
        bus.direction = direction;
        bus.channelCount = channels[index as usize] as i32;
        let name = match (is_main, is_input) {
            (true, true) => "Input",
            (true, false) => "Output",
            (false, _) => "Sidechain",
        };
        copy_wide(name, &mut bus.name);
        bus.busType = (if is_main {
            BusTypes_::kMain
        } else {
            BusTypes_::kAux
        }) as vst::BusType;
        // A sidechain is switched on by the host when something is routed
        // to it, so it does not start active.
        bus.flags = if is_main { default_active } else { 0 };
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
        media: vst::MediaType,
        direction: vst::BusDirection,
        index: i32,
        _state: TBool,
    ) -> tresult {
        // SAFETY: only reads constants of the plugin type.
        let count = unsafe { self.getBusCount(media, direction) };
        if (0..count).contains(&index) {
            kResultOk
        } else {
            kInvalidArgument
        }
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        guard(kResultFalse, || {
            // SAFETY: `setActive` runs on the main thread while the
            // component is not processing, so nothing else is using the
            // audio state.
            let audio = unsafe { &mut *self.audio.get() };
            if state != 0 {
                let max_block = audio.max_block.max(1);
                audio.buffer = AudioBuffer::new(P::LAYOUT.outputs, max_block);
                audio.sidechain = AudioBuffer::new(P::LAYOUT.sidechain, max_block);
                audio.events = EventBuffer::with_capacity(EVENTS_PER_BLOCK);
                audio.held = EventBuffer::with_capacity(HELD_EVENTS);
                audio.plugin.prepare(&ProcessSpec {
                    sample_rate: audio.sample_rate,
                    max_block_size: max_block,
                    num_channels: P::LAYOUT.outputs,
                });
                audio.prepared = true;
                let latency = u32::try_from(audio.plugin.latency_samples()).unwrap_or(u32::MAX);
                self.latency.store(latency, Ordering::Relaxed);
            } else {
                self.controls.running.store(false, Ordering::Relaxed);
                if audio.prepared {
                    audio.plugin.reset();
                    audio.held.clear();
                }
            }
            kResultOk
        })
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        guard(kResultFalse, || {
            // SAFETY: the host's stream, valid for this call, or null.
            let Some(stream) = (unsafe { ComRef::from_raw(state) }) else {
                return kInvalidArgument;
            };
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let mut count = 0i32;
                // SAFETY: the host's own stream, a buffer of exactly the
                // length passed, and an out-pointer to a local.
                let result = unsafe {
                    stream.read(chunk.as_mut_ptr().cast(), chunk.len() as i32, &mut count)
                };
                match usize::try_from(count) {
                    Ok(count) if count > 0 && count <= chunk.len() => {
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    _ => break,
                }
                if result != kResultOk || bytes.len() > MAX_STATE_BYTES {
                    break;
                }
            }
            let Ok(text) = std::str::from_utf8(&bytes) else {
                return kResultFalse;
            };
            // The parameters are atomics, so this is safe while audio
            // runs. The audio thread finds out at its next block, as
            // events.
            match load_state::<P>(&self.params, text) {
                Ok(_) => {
                    self.show_current_values();
                    kResultOk
                }
                Err(_) => kResultFalse,
            }
        })
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        guard(kResultFalse, || {
            // SAFETY: the host's stream, valid for this call, or null.
            let Some(stream) = (unsafe { ComRef::from_raw(state) }) else {
                return kInvalidArgument;
            };
            let text = save_state::<P>(&self.params);
            let mut rest = text.as_bytes();
            while !rest.is_empty() {
                let mut written = 0i32;
                let wanted = rest.len().min(i32::MAX as usize) as i32;
                // SAFETY: the host's own stream, a buffer of at least the
                // length passed, and an out-pointer to a local. The stream
                // only reads from the buffer.
                let result =
                    unsafe { stream.write(rest.as_ptr().cast_mut().cast(), wanted, &mut written) };
                match usize::try_from(written) {
                    Ok(count) if result == kResultOk && count > 0 && count <= rest.len() => {
                        rest = &rest[count..];
                    }
                    _ => return kResultFalse,
                }
            }
            kResultOk
        })
    }
}

impl<P: Plugin> vst::IAudioProcessorTrait for Component<P> {
    unsafe fn setBusArrangements(
        &self,
        inputs: *mut vst::SpeakerArrangement,
        num_inputs: i32,
        outputs: *mut vst::SpeakerArrangement,
        num_outputs: i32,
    ) -> tresult {
        // The layout is fixed by the plugin. A host may ask for another;
        // the answer is that only this one is available.
        let matches = |wanted: *mut vst::SpeakerArrangement, count: i32, is_input: bool| {
            let (channels, ours) = audio_buses::<P>(is_input);
            if count < 0 || count as usize != ours || (ours > 0 && wanted.is_null()) {
                return false;
            }
            (0..ours).all(|index| {
                // SAFETY: the host passes `count` arrangements, and `index`
                // is below `count`.
                unsafe { *wanted.add(index) == arrangement(channels[index]) }
            })
        };
        if matches(inputs, num_inputs, true) && matches(outputs, num_outputs, false) {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn getBusArrangement(
        &self,
        direction: vst::BusDirection,
        index: i32,
        arr: *mut vst::SpeakerArrangement,
    ) -> tresult {
        let is_input = direction == BusDirections_::kInput as vst::BusDirection;
        let (channels, count) = audio_buses::<P>(is_input);
        if arr.is_null() || index < 0 || index as usize >= count {
            return kInvalidArgument;
        }
        // SAFETY: a non-null pointer to a value the host owns.
        unsafe { *arr = arrangement(channels[index as usize]) };
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
        self.latency.load(Ordering::Relaxed)
    }

    unsafe fn setupProcessing(&self, setup: *mut vst::ProcessSetup) -> tresult {
        // SAFETY: a struct the host owns, valid for this call, or null.
        let Some(setup) = (unsafe { setup.as_ref() }) else {
            return kInvalidArgument;
        };
        if setup.symbolicSampleSize != vst::SymbolicSampleSizes_::kSample32 as i32 {
            return kResultFalse;
        }
        // SAFETY: `setupProcessing` runs on the main thread while the
        // component is inactive, so nothing else is using the audio state.
        let audio = unsafe { &mut *self.audio.get() };
        audio.sample_rate = setup.sampleRate;
        audio.max_block = setup.maxSamplesPerBlock.max(1) as usize;
        audio.prepared = false;
        kResultOk
    }

    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        self.controls.running.store(state != 0, Ordering::Relaxed);
        kResultOk
    }

    unsafe fn process(&self, data: *mut vst::ProcessData) -> tresult {
        guard(kResultFalse, || {
            // Numbers too small to hear are zero for as long as the plugin
            // runs, and the host's own setting is back when this returns.
            let _quiet = NoDenormals::new();
            // SAFETY: the host passes a `ProcessData` that is valid for
            // the duration of this call, or null.
            let Some(data) = (unsafe { data.as_mut() }) else {
                return kInvalidArgument;
            };
            // SAFETY: `process` runs on the audio thread, the only thread
            // using the audio state while the component is active.
            let audio = unsafe { &mut *self.audio.get() };
            if !audio.prepared {
                return kResultFalse;
            }

            // Not every host announces processing with `setProcessing`.
            self.controls.running.store(true, Ordering::Relaxed);

            let frames = data.numSamples.max(0) as usize;
            if frames > audio.buffer.capacity() {
                // More than the host promised in `setupProcessing`.
                return kResultFalse;
            }
            // SAFETY: the pointers in `data` are valid for this call.
            unsafe { self.gather_events(audio, data, frames > 0) };
            if frames == 0 {
                return kResultOk;
            }

            audio.buffer.set_num_frames(frames);
            audio.buffer.clear();
            // SAFETY: the bus pointers in `data` are valid for this call.
            unsafe {
                let inputs = buses(data.inputs, data.numInputs);
                if P::LAYOUT.inputs > 0 {
                    if let Some(main) = inputs.first() {
                        read_bus(main, &mut audio.buffer, P::LAYOUT.inputs);
                    }
                }
                if P::LAYOUT.sidechain > 0 {
                    audio.sidechain.set_num_frames(frames);
                    audio.sidechain.clear();
                    let index = usize::from(P::LAYOUT.inputs > 0);
                    if let Some(bus) = inputs.get(index) {
                        read_bus(bus, &mut audio.sidechain, P::LAYOUT.sidechain);
                    }
                }
            }

            // SAFETY: the context pointer is valid for this call, or null.
            let transport = unsafe { data.processContext.as_ref() }
                .map_or(Transport::STOPPED, convert_transport);
            let mut context = ProcessContext::new(&audio.events, &transport);
            if P::LAYOUT.sidechain > 0 {
                context = context.with_sidechain(&audio.sidechain);
            }
            audio.plugin.process(&mut audio.buffer, &context);

            // SAFETY: the bus pointers in `data` are valid for this call.
            // Everything was read from the inputs before this point, so
            // writing is sound even when the host processes in place.
            unsafe {
                for (index, bus) in buses(data.outputs, data.numOutputs).iter_mut().enumerate() {
                    write_bus(bus, (index == 0).then_some(&audio.buffer), frames);
                }
            }
            kResultOk
        })
    }

    unsafe fn getTailSamples(&self) -> u32 {
        // The plugin API has no way to state its tail yet, so the host is
        // told never to assume silence. Nothing is cut off; the cost is
        // that the host keeps processing a silent plugin.
        vst::kInfiniteTail
    }
}

impl<P: Plugin> vst::IEditControllerTrait for Component<P> {
    unsafe fn setComponentState(&self, _state: *mut IBStream) -> tresult {
        // The component is this same object and already has its state, so
        // there is nothing to read; the controller only has to report it.
        self.show_current_values();
        kResultOk
    }

    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getParameterCount(&self) -> i32 {
        (self.controls.ids.len() as u32 + Self::midi_param_count()) as i32
    }

    unsafe fn getParameterInfo(&self, index: i32, info: *mut vst::ParameterInfo) -> tresult {
        use vst::ParameterInfo_::ParameterFlags_ as flags;
        // SAFETY: a struct the host owns, or null.
        let (Some(info), Ok(index)) = (unsafe { info.as_mut() }, usize::try_from(index)) else {
            return kInvalidArgument;
        };

        if let Some(&(own, id)) = self.controls.ids.get(index) {
            let Some(param) = self.params.find_param(own) else {
                return kInvalidArgument;
            };
            let details = param.info().flags();
            let mut bits = 0;
            if details.automatable {
                bits |= flags::kCanAutomate as i32;
            }
            if details.hidden {
                bits |= flags::kIsHidden as i32;
            }
            if details.bypass {
                bits |= flags::kIsBypass as i32;
            }
            info.id = id;
            copy_wide(param.name(), &mut info.title);
            copy_wide(param.name(), &mut info.shortTitle);
            copy_wide(param.info().unit(), &mut info.units);
            info.stepCount = param
                .step_count()
                .map_or(0, |steps| steps.min(i32::MAX as u32) as i32);
            info.defaultNormalizedValue = param.default_normalized();
            info.unitId = vst::kRootUnitId;
            info.flags = bits;
            return kResultOk;
        }

        // The hidden parameters that carry MIDI controllers.
        let midi = (index - self.controls.ids.len()) as u32;
        if midi >= Self::midi_param_count() {
            return kInvalidArgument;
        }
        let (channel, controller) = (midi / MIDI_CONTROLLERS, midi % MIDI_CONTROLLERS);
        let is_bend = controller == vst::ControllerNumbers_::kPitchBend as u32;
        info.id = MIDI_PARAM_BASE + midi;
        // Built without allocating: a host asks for thousands of these.
        let title = midi_title(channel + 1, controller);
        copy_wide(title.as_str(), &mut info.title);
        copy_wide(title.as_str(), &mut info.shortTitle);
        copy_wide("", &mut info.units);
        info.stepCount = 0;
        info.defaultNormalizedValue = if is_bend { 0.5 } else { 0.0 };
        info.unitId = MIDI_UNIT;
        info.flags = flags::kIsHidden as i32;
        kResultOk
    }

    unsafe fn getParamStringByValue(
        &self,
        id: u32,
        value: f64,
        string: *mut vst::String128,
    ) -> tresult {
        guard(kResultFalse, || {
            // SAFETY: a buffer the host owns, or null.
            let (Some(param), Some(out)) = (self.param(id), unsafe { string.as_mut() }) else {
                return kInvalidArgument;
            };
            copy_wide(&param.plain_to_text(param.normalized_to_plain(value)), out);
            kResultOk
        })
    }

    unsafe fn getParamValueByString(
        &self,
        id: u32,
        string: *mut vst::TChar,
        value: *mut f64,
    ) -> tresult {
        guard(kResultFalse, || {
            // SAFETY: the host passes a zero-terminated string, or null.
            let (Some(param), Some(text)) = (self.param(id), unsafe { read_wide(string) }) else {
                return kInvalidArgument;
            };
            let Some(plain) = param.text_to_plain(&text) else {
                return kResultFalse;
            };
            if value.is_null() {
                return kInvalidArgument;
            }
            // SAFETY: a non-null pointer to a value the host owns.
            unsafe { *value = param.plain_to_normalized(plain) };
            kResultOk
        })
    }

    unsafe fn normalizedParamToPlain(&self, id: u32, value: f64) -> f64 {
        self.param(id)
            .map_or(value, |param| param.normalized_to_plain(value))
    }

    unsafe fn plainParamToNormalized(&self, id: u32, plain: f64) -> f64 {
        self.param(id)
            .map_or(plain, |param| param.plain_to_normalized(plain))
    }

    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        match self.index_of(id) {
            Some(index) => self.controls.shown[index].load(Ordering::Relaxed),
            // A controller parameter has no stored value. Pitch bend rests
            // in the middle; everything else rests at zero.
            None => {
                let controller = id.wrapping_sub(MIDI_PARAM_BASE) % MIDI_CONTROLLERS;
                let is_midi = Self::midi_from_param(id, 0.0).is_some();
                if is_midi && controller == vst::ControllerNumbers_::kPitchBend as u32 {
                    0.5
                } else {
                    0.0
                }
            }
        }
    }

    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        if value.is_nan() {
            return kInvalidArgument;
        }
        let Some(index) = self.index_of(id) else {
            // Controller moves reach the plugin through `process`.
            return if Self::midi_from_param(id, value).is_some() {
                kResultOk
            } else {
                kInvalidArgument
            };
        };
        // See the module documentation: while blocks are running the same
        // change arrives in `process`, on its frame, and that is the one
        // the plugin follows.
        self.controls.set_normalized(index, value);
        kResultOk
    }

    unsafe fn setComponentHandler(&self, handler: *mut vst::IComponentHandler) -> tresult {
        guard(kResultFalse, || {
            // SAFETY: the host's object, valid for this call, or null. The
            // copy takes a reference of its own, which `Handler` releases
            // when the host replaces or takes away the handler, at
            // `terminate`, or when the component goes away.
            let handler = unsafe { ComRef::from_raw(handler) }.map(|handler| handler.to_com_ptr());
            self.handler.set(handler);
            kResultOk
        })
    }

    unsafe fn createView(&self, name: sb::FIDString) -> *mut sb::IPlugView {
        guard(std::ptr::null_mut(), || match &self.editors {
            // SAFETY: the host passes a NUL-terminated string, or null.
            Some(editors) => unsafe { editors.create_view(name) },
            // A plugin without an editor has no view to give.
            None => std::ptr::null_mut(),
        })
    }
}

impl<P: Plugin> Drop for Component<P> {
    fn drop(&mut self) {
        // A host that released the component without `terminate`, or with
        // a view of it still open: the editors are closed, and the host's
        // handler released, while the host can still be called.
        guard((), || self.shut_down());
    }
}

/// A short piece of text built without allocating.
struct FixedText {
    bytes: [u8; 24],
    len: usize,
}

impl FixedText {
    fn as_str(&self) -> &str {
        // Only whole `&str`s are ever appended.
        std::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl std::fmt::Write for FixedText {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        let end = self.len + text.len();
        let slot = self.bytes.get_mut(self.len..end).ok_or(std::fmt::Error)?;
        slot.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// A title such as `"MIDI 3 CC 74"` for a hidden controller parameter.
fn midi_title(channel: u32, controller: u32) -> FixedText {
    let mut text = FixedText {
        bytes: [0; 24],
        len: 0,
    };
    // The longest title is 18 bytes, so writing cannot fail.
    let _ = if controller == vst::ControllerNumbers_::kPitchBend as u32 {
        write!(text, "MIDI {channel} Pitch Bend")
    } else if controller == vst::ControllerNumbers_::kAfterTouch as u32 {
        write!(text, "MIDI {channel} Pressure")
    } else {
        write!(text, "MIDI {channel} CC {controller}")
    };
    text
}

impl<P: Plugin> vst::IMidiMappingTrait for Component<P> {
    unsafe fn getMidiControllerAssignment(
        &self,
        bus: i32,
        channel: i16,
        controller: vst::CtrlNumber,
        id: *mut vst::ParamID,
    ) -> tresult {
        let (Ok(channel), Ok(controller)) = (u32::try_from(channel), u32::try_from(controller))
        else {
            return kResultFalse;
        };
        if bus != 0
            || id.is_null()
            || Self::midi_param_count() == 0
            || channel >= MIDI_CHANNELS
            || controller >= MIDI_CONTROLLERS
        {
            return kResultFalse;
        }
        // SAFETY: a non-null pointer to a value the host owns.
        unsafe { *id = MIDI_PARAM_BASE + channel * MIDI_CONTROLLERS + controller };
        kResultOk
    }
}

impl<P: Plugin> vst::INoteExpressionControllerTrait for Component<P> {
    unsafe fn getNoteExpressionCount(&self, bus: i32, _channel: i16) -> i32 {
        if bus == 0 && takes_notes::<P>() {
            EXPRESSIONS.len() as i32
        } else {
            0
        }
    }

    unsafe fn getNoteExpressionInfo(
        &self,
        bus: i32,
        channel: i16,
        index: i32,
        info: *mut vst::NoteExpressionTypeInfo,
    ) -> tresult {
        // SAFETY: only reads constants of the plugin type.
        let count = unsafe { self.getNoteExpressionCount(bus, channel) };
        // SAFETY: a struct the host owns, or null.
        let (Some(info), true) = (unsafe { info.as_mut() }, (0..count).contains(&index)) else {
            return kInvalidArgument;
        };
        let (type_id, title, short) = EXPRESSIONS[index as usize];
        let is_tuning = type_id == vst::NoteExpressionTypeIDs_::kTuningTypeID as u32;
        let is_pan = type_id == vst::NoteExpressionTypeIDs_::kPanTypeID as u32;
        let is_volume = type_id == vst::NoteExpressionTypeIDs_::kVolumeTypeID as u32;
        info.typeId = type_id;
        copy_wide(title, &mut info.title);
        copy_wide(short, &mut info.shortTitle);
        copy_wide(if is_tuning { "st" } else { "" }, &mut info.units);
        info.unitId = -1;
        info.valueDesc = vst::NoteExpressionValueDescription {
            defaultValue: if is_volume {
                0.25
            } else if is_tuning || is_pan {
                0.5
            } else {
                0.0
            },
            minimum: 0.0,
            maximum: 1.0,
            stepCount: 0,
        };
        info.associatedParameterId = vst::kNoParamId;
        info.flags = if is_tuning || is_pan {
            vst::NoteExpressionTypeInfo_::NoteExpressionTypeFlags_::kIsBipolar as i32
        } else {
            0
        };
        kResultOk
    }

    unsafe fn getNoteExpressionStringByValue(
        &self,
        _bus: i32,
        _channel: i16,
        id: vst::NoteExpressionTypeID,
        value: f64,
        string: *mut vst::String128,
    ) -> tresult {
        guard(kResultFalse, || {
            // SAFETY: a buffer the host owns, or null.
            let (Some((_, plain)), Some(out)) =
                (expression_from_vst(id, value), unsafe { string.as_mut() })
            else {
                return kInvalidArgument;
            };
            copy_wide(&format!("{plain:.2}"), out);
            kResultOk
        })
    }

    unsafe fn getNoteExpressionValueByString(
        &self,
        _bus: i32,
        _channel: i16,
        id: vst::NoteExpressionTypeID,
        string: *const vst::TChar,
        value: *mut f64,
    ) -> tresult {
        guard(kResultFalse, || {
            // SAFETY: the host passes a zero-terminated string, or null.
            let Some(text) = (unsafe { read_wide(string) }) else {
                return kInvalidArgument;
            };
            // A number, with or without the unit after it.
            let number = text
                .trim()
                .trim_end_matches(|c: char| !c.is_ascii_digit() && c != '.');
            let Ok(plain) = number.trim().parse::<f64>() else {
                return kResultFalse;
            };
            let (Some(position), false) = (expression_to_vst(id, plain), value.is_null()) else {
                return kInvalidArgument;
            };
            // SAFETY: a non-null pointer to a value the host owns.
            unsafe { *value = position };
            kResultOk
        })
    }
}

impl<P: Plugin> vst::IUnitInfoTrait for Component<P> {
    unsafe fn getUnitCount(&self) -> i32 {
        // The root, and the controllers' unit if there are any.
        1 + i32::from(Self::midi_param_count() > 0)
    }

    unsafe fn getUnitInfo(&self, index: i32, info: *mut vst::UnitInfo) -> tresult {
        // SAFETY: only reads constants of the plugin type.
        let count = unsafe { self.getUnitCount() };
        // SAFETY: a struct the host owns, or null.
        let (Some(info), true) = (unsafe { info.as_mut() }, (0..count).contains(&index)) else {
            return kInvalidArgument;
        };
        if index == 0 {
            info.id = vst::kRootUnitId;
            info.parentUnitId = vst::kNoParentUnitId;
            copy_wide("Root", &mut info.name);
        } else {
            info.id = MIDI_UNIT;
            info.parentUnitId = vst::kRootUnitId;
            copy_wide("MIDI Controllers", &mut info.name);
        }
        info.programListId = vst::kNoProgramListId;
        kResultOk
    }

    // There are no program lists: presets are the host's, through state.

    unsafe fn getProgramListCount(&self) -> i32 {
        0
    }

    unsafe fn getProgramListInfo(&self, _index: i32, _info: *mut vst::ProgramListInfo) -> tresult {
        kInvalidArgument
    }

    unsafe fn getProgramName(
        &self,
        _list: vst::ProgramListID,
        _program: i32,
        _name: *mut vst::String128,
    ) -> tresult {
        kInvalidArgument
    }

    unsafe fn getProgramInfo(
        &self,
        _list: vst::ProgramListID,
        _program: i32,
        _attribute: vst::CString,
        _value: *mut vst::String128,
    ) -> tresult {
        kInvalidArgument
    }

    unsafe fn hasProgramPitchNames(&self, _list: vst::ProgramListID, _program: i32) -> tresult {
        kResultFalse
    }

    unsafe fn getProgramPitchName(
        &self,
        _list: vst::ProgramListID,
        _program: i32,
        _pitch: i16,
        _name: *mut vst::String128,
    ) -> tresult {
        kResultFalse
    }

    unsafe fn getSelectedUnit(&self) -> vst::UnitID {
        self.selected_unit.load(Ordering::Relaxed)
    }

    unsafe fn selectUnit(&self, unit: vst::UnitID) -> tresult {
        // SAFETY: only reads constants of the plugin type.
        if (0..unsafe { self.getUnitCount() }).contains(&unit) {
            self.selected_unit.store(unit, Ordering::Relaxed);
            kResultOk
        } else {
            kInvalidArgument
        }
    }

    unsafe fn getUnitByBus(
        &self,
        _media: vst::MediaType,
        _direction: vst::BusDirection,
        _bus: i32,
        _channel: i32,
        _unit: *mut vst::UnitID,
    ) -> tresult {
        // No bus is tied to a unit.
        kResultFalse
    }

    unsafe fn setUnitProgramData(
        &self,
        _list_or_unit: i32,
        _program: i32,
        _data: *mut IBStream,
    ) -> tresult {
        kNotImplemented
    }
}

impl<P: Plugin> vst::IProcessContextRequirementsTrait for Component<P> {
    unsafe fn getProcessContextRequirements(&self) -> u32 {
        use vst::IProcessContextRequirements_::Flags_ as need;
        (need::kNeedTempo
            | need::kNeedTimeSignature
            | need::kNeedProjectTimeMusic
            | need::kNeedBarPositionMusic
            | need::kNeedCycleMusic
            | need::kNeedTransportState) as u32
    }
}

/// Creates a component and hands it out through the interface `iid`.
///
/// # Safety
///
/// `iid` must point to a 16-byte interface id and `obj` to a pointer the
/// caller owns.
pub(crate) unsafe fn create<P: Plugin>(iid: *const TUID, obj: *mut *mut c_void) -> tresult {
    let Some(component) = guard(None, Component::<P>::new) else {
        return kResultFalse;
    };
    let wrapper = vst3::ComWrapper::new(component);
    let Some(unknown) = wrapper.to_com_ptr::<FUnknown>() else {
        return kResultFalse;
    };
    let pointer = unknown.as_ptr();
    // SAFETY: a live object of ours, and the caller's pointers. The query
    // takes its own reference; ours is released when `unknown` drops.
    unsafe { ((*(*pointer).vtbl).queryInterface)(pointer, iid, obj) }
}
