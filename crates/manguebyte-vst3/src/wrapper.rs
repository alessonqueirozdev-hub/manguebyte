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
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};

use manguebyte_core::{AtomicF64, AudioBuffer, NoDenormals, ParamId};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::note_input::NoteInput;
use manguebyte_dsp::{ProcessContext, ProcessSpec, Transport};
use manguebyte_midi::MidiMessage;
use manguebyte_params::{Param, ParamSet};
use manguebyte_plugin::{
    AudioLayout, AuxOutputs, MidiController, MidiControllers, ParamWatcher, Plugin, PluginKind,
    apply_param_events, load_state, offered_layouts, save_state,
};
use vst3::Steinberg::Vst::{self as vst, BusDirections_, BusTypes_, MediaTypes_};
use vst3::Steinberg::{
    self as sb, FUnknown, IBStream, IBStreamTrait, IPluginBaseTrait, TBool, TUID, kInvalidArgument,
    kNotImplemented, kResultFalse, kResultOk, tresult,
};
use vst3::{Class, ComRef};

use crate::handler::Handler;
use crate::output::NoteSender;
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
/// them declares a hidden parameter for each, and the host moves those
/// parameters. Their ids are reserved for 130 controllers (the 128 of MIDI,
/// channel pressure and pitch bend, numbered as [`MidiController::index`]
/// numbers them) on each of 16 channels: the controller with index `c` on
/// channel `ch` is `MIDI_PARAM_BASE + ch * 130 + c`. Only the controllers
/// the plugin declares in [`Plugin::MIDI_CONTROLLERS`] are listed; a plugin
/// that did not ask for channels gets the channel 0 ids alone, every
/// channel is mapped to them, and each of their moves is delivered on all
/// 16 channels.
const MIDI_PARAM_BASE: u32 = 0x7F00_0000;
const MIDI_CONTROLLERS: u32 = vst::ControllerNumbers_::kCountCtrlNumber as u32;
const MIDI_CHANNELS: u32 = 16;
const MIDI_PARAM_SPAN: u32 = MIDI_CONTROLLERS * MIDI_CHANNELS;

// The plugin API numbers controllers as VST 3 does.
const _: () = {
    assert!(MIDI_CONTROLLERS as usize == MidiController::COUNT);
    assert!(vst::ControllerNumbers_::kAfterTouch as u32 == 128);
    assert!(vst::ControllerNumbers_::kPitchBend as u32 == 129);
};

/// The unit the hidden controller parameters are filed under, so that a
/// host which lists parameters by unit keeps them apart from the plugin's
/// own.
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
pub(crate) fn expression_to_vst(type_id: u32, plain: f64) -> Option<f64> {
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
    takes_notes_const(P::INFO.kind)
}

/// Whether a plugin of this kind has an event bus.
const fn takes_notes_const(kind: PluginKind) -> bool {
    matches!(kind, PluginKind::Instrument | PluginKind::NoteEffect)
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

/// The channel counts of the audio buses in one direction under `layout`,
/// main bus first, and how many buses there are.
///
/// Which buses there are is fixed by the plugin's own layout, since VST 3
/// fixes the bus count; `layout`, one of those it offers, gives their
/// widths, and a sidechain it leaves out is a bus with no channels.
fn audio_buses<P: Plugin>(layout: AudioLayout, is_input: bool) -> ([usize; 2], usize) {
    let base = P::LAYOUT;
    let candidates = if is_input {
        [
            (base.inputs, layout.inputs),
            (base.sidechain, layout.sidechain),
        ]
    } else {
        [(base.outputs, layout.outputs), (0, 0)]
    };
    let mut widths = [0; 2];
    let mut count = 0;
    for (present, width) in candidates {
        if present > 0 {
            widths[count] = width;
            count += 1;
        }
    }
    (widths, count)
}

/// How many audio buses there are in one direction: those of
/// [`audio_buses`], and after the main output the auxiliary ones.
fn audio_bus_count<P: Plugin>(is_input: bool) -> usize {
    let aux = if is_input { 0 } else { P::AUX_OUTPUTS.len() };
    audio_buses::<P>(P::LAYOUT, is_input).1 + aux
}

/// The width of audio bus `index` in one direction under `layout`, or
/// `None` past the last bus. An auxiliary output has its fixed width.
fn audio_bus_width<P: Plugin>(layout: AudioLayout, is_input: bool, index: usize) -> Option<usize> {
    let (widths, count) = audio_buses::<P>(layout, is_input);
    if index < count {
        return Some(widths[index]);
    }
    if is_input {
        return None;
    }
    P::AUX_OUTPUTS.get(index - count).map(|aux| aux.channels)
}

/// The index among the auxiliary outputs of audio output bus `index`, for
/// one that is auxiliary.
fn aux_bus<P: Plugin>(index: usize) -> Option<usize> {
    let mains = usize::from(P::LAYOUT.outputs > 0);
    index
        .checked_sub(mains)
        .filter(|&aux| aux < P::AUX_OUTPUTS.len())
}

/// The index of the sidechain among the audio input buses, for a plugin
/// that has one.
fn sidechain_bus<P: Plugin>() -> Option<usize> {
    (P::LAYOUT.sidechain > 0).then_some(usize::from(P::LAYOUT.inputs > 0))
}

/// The state used while processing.
struct Audio<P> {
    plugin: P,
    sample_rate: f64,
    max_block: usize,
    /// Whether the plugin has been prepared for the current setup.
    prepared: bool,
    /// The layout the plugin was prepared for.
    layout: AudioLayout,
    buffer: AudioBuffer<f32>,
    sidechain: AudioBuffer<f32>,
    /// The auxiliary output buses, switched as `Component::aux_active`
    /// was when the component was activated.
    aux: AuxOutputs,
    events: EventBuffer,
    /// Controller moves and notes that arrived in a block with no audio,
    /// for the next block that has some.
    held: EventBuffer,
    /// What the host sent for the coming block, as it sent it, in time
    /// order, before it is read into `events`.
    raw: EventBuffer,
    /// Reads notes on the channels of an MPE zone, and the controller
    /// moves the hidden parameters stand for, as every wrapper does.
    input: NoteInput,
    /// What the plugin sends out, on its way to the output event bus.
    notes: NoteSender,
    /// Parameter values as the plugin last saw them.
    watcher: ParamWatcher,
    /// Whether the plugin panicked in the last block, so that the next one
    /// with audio starts by announcing every parameter's value: the changes
    /// of the block it did not finish were stored and remembered as seen,
    /// but may never have reached it.
    resend_values: bool,
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
    /// The layouts a host may arrange the buses in, the plugin's own first.
    layouts: Vec<AudioLayout>,
    /// The one `setBusArrangements` chose, by index into `layouts`. Only
    /// changed while the component is inactive.
    config: AtomicUsize,
    /// Whether the host has switched the sidechain bus on (`activateBus`).
    /// It starts off, as its bus info says.
    sidechain_active: AtomicBool,
    /// Whether the host has switched each auxiliary output bus on, in the
    /// order of [`Plugin::AUX_OUTPUTS`]. Each starts as declared.
    aux_active: Box<[AtomicBool]>,
    /// Whether the component is active, so that buses are not rearranged
    /// under it.
    active: AtomicBool,
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
            .any(|&id| (MIDI_PARAM_BASE..MIDI_PARAM_BASE + MIDI_PARAM_SPAN).contains(&id));
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
            layouts: offered_layouts::<P>(),
            config: AtomicUsize::new(0),
            sidechain_active: AtomicBool::new(false),
            aux_active: P::AUX_OUTPUTS
                .iter()
                .map(|aux| AtomicBool::new(aux.active_by_default))
                .collect(),
            active: AtomicBool::new(false),
            selected_unit: AtomicI32::new(vst::kRootUnitId),
            audio: UnsafeCell::new(Audio {
                watcher: ParamWatcher::new(&*params),
                plugin,
                sample_rate: 44_100.0,
                max_block: 0,
                prepared: false,
                layout: P::LAYOUT,
                buffer: AudioBuffer::new(P::LAYOUT.outputs, 0),
                sidechain: AudioBuffer::new(P::LAYOUT.sidechain, 0),
                aux: AuxOutputs::new(),
                events: EventBuffer::new(),
                held: EventBuffer::new(),
                raw: EventBuffer::new(),
                input: NoteInput::new(P::MPE),
                notes: NoteSender::new(if P::NOTE_OUTPUT { EVENTS_PER_BLOCK } else { 0 }),
                resend_values: false,
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

    /// The layout `setBusArrangements` chose, which the buses report.
    fn selected(&self) -> AudioLayout {
        let index = self.config.load(Ordering::Relaxed);
        self.layouts.get(index).copied().unwrap_or(P::LAYOUT)
    }

    /// The layout the plugin runs with: the selected one, or, while the
    /// sidechain bus is off, the same without a sidechain if the plugin
    /// offers that. Otherwise the sidechain stays, silent.
    fn running_layout(&self) -> AudioLayout {
        let selected = self.selected();
        let without = selected.with_sidechain(0);
        if selected.sidechain > 0
            && !self.sidechain_active.load(Ordering::Relaxed)
            && self.layouts.contains(&without)
        {
            without
        } else {
            selected
        }
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

    /// The controllers the plugin listens to; none if it has no event bus.
    const MIDI: MidiControllers = if takes_notes_const(P::INFO.kind) {
        P::MIDI_CONTROLLERS
    } else {
        MidiControllers::NONE
    };

    /// How many channels have hidden parameters of their own: 16 for a
    /// plugin that asked for channels, otherwise one, shared by all.
    const MIDI_PARAM_CHANNELS: u32 = if Self::MIDI.is_per_channel() {
        MIDI_CHANNELS
    } else {
        1
    };

    /// How many hidden parameters stand in for MIDI controllers.
    fn midi_param_count() -> u32 {
        Self::MIDI.len() as u32 * Self::MIDI_PARAM_CHANNELS
    }

    /// The id of the hidden parameter for `controller` on `channel`, which
    /// must be below [`Self::MIDI_PARAM_CHANNELS`].
    fn midi_param_id(channel: u32, controller: MidiController) -> u32 {
        MIDI_PARAM_BASE + channel * MIDI_CONTROLLERS + controller.index() as u32
    }

    /// The channel and controller of the hidden parameter `id`, if the
    /// plugin has one with that id.
    fn midi_param(id: u32) -> Option<(u8, MidiController)> {
        let index = id.checked_sub(MIDI_PARAM_BASE)?;
        if index >= MIDI_PARAM_SPAN {
            return None;
        }
        let channel = index / MIDI_CONTROLLERS;
        let controller = MidiController::from_index((index % MIDI_CONTROLLERS) as usize)?;
        (channel < Self::MIDI_PARAM_CHANNELS && Self::MIDI.contains(controller))
            .then_some((channel as u8, controller))
    }

    /// The channel and controller of the hidden parameter at `position`
    /// among the hidden ones: channel by channel, and within a channel in
    /// the order of the controllers' indices.
    fn midi_param_at(position: u32) -> Option<(u32, MidiController)> {
        let per_channel = Self::MIDI.len() as u32;
        if per_channel == 0 {
            return None;
        }
        let channel = position / per_channel;
        if channel >= Self::MIDI_PARAM_CHANNELS {
            return None;
        }
        let controller = Self::MIDI.get((position % per_channel) as usize)?;
        Some((channel, controller))
    }

    /// The MIDI message a move of a hidden controller parameter stands for,
    /// on the parameter's channel (0 for one shared by every channel). A
    /// value that is not a number stands for nothing.
    fn midi_from_param(id: u32, value: f64) -> Option<MidiMessage> {
        let (channel, controller) = Self::midi_param(id)?;
        if value.is_nan() {
            return None;
        }
        let value = value.clamp(0.0, 1.0);
        let seven_bits = (value * 127.0).round() as u8;
        Some(match controller.control_number() {
            Some(number) => MidiMessage::ControlChange {
                channel,
                controller: number,
                value: seven_bits,
            },
            None if controller == MidiController::PITCH_BEND => MidiMessage::PitchBend {
                channel,
                value: ((value * 16_383.0).round() as i32 - 8_192) as i16,
            },
            None => MidiMessage::ChannelPressure {
                channel,
                pressure: seven_bits,
            },
        })
    }

    /// Queues the message of a controller move on `offset`: once, or, for a
    /// plugin whose controllers are shared by every channel, once on each
    /// of the 16 channels in channel order. The host has said which
    /// channel the move came from only to `IMidiMapping`, which gave the
    /// same parameter for all of them, so a plugin that keeps controllers
    /// per channel (as `Synth` does) has to hear it on every channel to
    /// apply it to every note. A full buffer drops what does not fit.
    ///
    /// Each message is read through `input`, so that on the channels of an
    /// MPE zone the pitch bend, pressure and controller 74 of a member
    /// channel become per-note expressions. That needs the channel the
    /// move came from: an MPE instrument declares its controllers
    /// [`per_channel`](MidiControllers::per_channel).
    /// Queues one event as the host sent it, reading notes and controller
    /// moves through `input`. Everything else is queued as it is.
    fn read(input: &mut NoteInput, event: Event, events: &mut EventBuffer) {
        let offset = event.offset;
        let mut push = |event| {
            let _ = events.push(event);
        };
        match event.kind {
            EventKind::Midi(message) if MidiController::of(&message).is_some() => {
                Self::push_controller(input, events, offset, message);
            }
            EventKind::NoteOn { note, velocity } => input.note_on(offset, note, velocity, push),
            EventKind::NoteOff { note, velocity } => input.note_off(offset, note, velocity, push),
            _ => push(event),
        }
    }

    fn push_controller(
        input: &mut NoteInput,
        events: &mut EventBuffer,
        offset: u32,
        message: MidiMessage,
    ) {
        let mut push = |event| {
            let _ = events.push(event);
        };
        if Self::MIDI.is_per_channel() {
            input.midi1(offset, message, &mut push);
            return;
        }
        for channel in 0..MIDI_CHANNELS as u8 {
            input.midi1(offset, on_channel(message, channel), &mut push);
        }
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
            // What blocks without audio left behind goes first. Controller
            // moves were held once each, and are spread over the channels
            // now.
            for &event in &audio.held {
                Self::read(&mut audio.input, event, &mut audio.events);
            }
            audio.held.clear();
            // Then changes the plugin has not seen yet, on frame 0: a
            // restored state, or a value set while no blocks were running.
            // After a block the plugin did not finish, that is every value.
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
                            // Queued once, and spread over the channels
                            // when it is read below, or when it is
                            // delivered after a block with no audio.
                            Event::from_midi(offset, message).kind
                        } else {
                            continue;
                        };
                        let _ = audio.raw.push(Event { offset, kind });
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
                        convert_event(&event, &mut audio.raw);
                    }
                }
            }
        }

        if has_audio {
            // The host's parameter changes and its events come in two
            // lists, and are now in one, in time order: that is the order
            // the notes and controllers have to be read in.
            let Audio {
                raw, input, events, ..
            } = audio;
            for &event in raw.iter() {
                Self::read(input, event, events);
            }
            raw.clear();
            // Only the host's values are stored. The announced ones were
            // read from the parameters, and the main thread may have stored
            // a newer value since (a state load, or an editor's edit while
            // no blocks ran); writing the old one back would undo it.
            let from_host = &audio.events.as_slice()[announced..];
            audio.watcher.apply_and_remember(params, from_host);
            return;
        }
        apply_param_events(params, &audio.raw);
        // The values are stored, and the watcher has not been told, so it
        // announces them on the next block with audio. A controller move
        // or a note has no stored value to be found again, so it is kept,
        // as the host sent it, to be read when it is delivered.
        for event in &audio.raw {
            if !matches!(event.kind, EventKind::ParamValue { .. }) {
                let kind = event.kind;
                let _ = audio.held.push(Event { offset: 0, kind });
            }
        }
        audio.raw.clear();
    }
}

/// `message`, a controller move, on `channel` instead of its own. Other
/// messages are returned unchanged.
fn on_channel(message: MidiMessage, channel: u8) -> MidiMessage {
    match message {
        MidiMessage::ControlChange {
            controller, value, ..
        } => MidiMessage::ControlChange {
            channel,
            controller,
            value,
        },
        MidiMessage::PitchBend { value, .. } => MidiMessage::PitchBend { channel, value },
        MidiMessage::ChannelPressure { pressure, .. } => {
            MidiMessage::ChannelPressure { channel, pressure }
        }
        other => other,
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
        // VST 3 gives neither the time in seconds, the bar's number nor
        // the tempo's slope.
        position_seconds: None,
        bar_number: None,
        tempo_increment: None,
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
/// With `wide`, the host processes in 64 bits (`kSample64`) and the bus
/// holds `channelBuffers64`, whose samples are rounded to the nearest
/// `f32`: the plugin hears exactly what it would have been given in 32
/// bits.
///
/// # Safety
///
/// `bus` must hold valid channel pointers of the size `wide` says, each
/// with at least `buffer.num_frames()` frames, or a null channel array.
unsafe fn read_bus(
    bus: &vst::AudioBusBuffers,
    buffer: &mut AudioBuffer<f32>,
    channels: usize,
    wide: bool,
) {
    let frames = buffer.num_frames();
    let count = channels
        .min(bus.numChannels.max(0) as usize)
        .min(buffer.num_channels());
    if wide {
        // SAFETY: the host processes in 64 bits, so this is the member of
        // the union it filled in.
        let pointers = unsafe { bus.__field0.channelBuffers64 };
        if pointers.is_null() {
            return;
        }
        for index in 0..count {
            // SAFETY: `index` is below the bus's channel count.
            let channel = unsafe { *pointers.add(index) };
            if channel.is_null() {
                continue;
            }
            // SAFETY: the host guarantees `frames` readable samples.
            let samples = unsafe { std::slice::from_raw_parts(channel, frames) };
            for (out, &sample) in buffer.channel_mut(index).iter_mut().zip(samples) {
                *out = sample as f32;
            }
        }
        return;
    }
    // SAFETY: the host processes in 32 bits, so this is the member of the
    // union it filled in.
    let pointers = unsafe { bus.__field0.channelBuffers32 };
    if pointers.is_null() {
        return;
    }
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
/// channel for it. With `wide`, in 64 bits, the plugin's samples widened,
/// which is exact.
///
/// # Safety
///
/// `bus` must hold valid channel pointers of the size `wide` says, each
/// with at least `frames` writable frames, or a null channel array.
unsafe fn write_bus(
    bus: &mut vst::AudioBusBuffers,
    source: Option<&AudioBuffer<f32>>,
    frames: usize,
    wide: bool,
) {
    let channels = bus.numChannels.max(0) as usize;
    let mut silent = 0u64;
    for index in 0..channels {
        let from = source
            .filter(|buffer| index < buffer.num_channels())
            .map(|buffer| buffer.channel(index));
        if from.is_none() {
            silent |= 1 << index.min(63);
        }
        if wide {
            // SAFETY: as in `read_bus`.
            let pointers = unsafe { bus.__field0.channelBuffers64 };
            if pointers.is_null() {
                break;
            }
            // SAFETY: `index` is below the bus's channel count.
            let channel = unsafe { *pointers.add(index) };
            if channel.is_null() {
                continue;
            }
            // SAFETY: the host guarantees `frames` writable samples per
            // channel, and no reference to the host's input is alive here.
            let samples = unsafe { std::slice::from_raw_parts_mut(channel, frames) };
            match from {
                Some(from) => {
                    for (out, &sample) in samples.iter_mut().zip(from) {
                        *out = f64::from(sample);
                    }
                }
                None => samples.fill(0.0),
            }
        } else {
            // SAFETY: as in `read_bus`.
            let pointers = unsafe { bus.__field0.channelBuffers32 };
            if pointers.is_null() {
                break;
            }
            // SAFETY: `index` is below the bus's channel count.
            let channel = unsafe { *pointers.add(index) };
            if channel.is_null() {
                continue;
            }
            // SAFETY: as above.
            let samples = unsafe { std::slice::from_raw_parts_mut(channel, frames) };
            match from {
                Some(from) => samples.copy_from_slice(from),
                None => samples.fill(0.0),
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
            audio_bus_count::<P>(is_input) as i32
        } else if media == MediaTypes_::kEvent as vst::MediaType {
            i32::from(if is_input {
                takes_notes::<P>()
            } else {
                P::NOTE_OUTPUT
            })
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
            let present = if is_input {
                takes_notes::<P>()
            } else {
                P::NOTE_OUTPUT
            };
            if index != 0 || !present {
                return kInvalidArgument;
            }
            bus.mediaType = media;
            bus.direction = direction;
            bus.channelCount = MIDI_CHANNELS as i32;
            copy_wide(if is_input { "Notes" } else { "Notes out" }, &mut bus.name);
            bus.busType = BusTypes_::kMain as vst::BusType;
            bus.flags = default_active;
            return kResultOk;
        }
        if media != MediaTypes_::kAudio as vst::MediaType {
            return kInvalidArgument;
        }

        let Some(width) = usize::try_from(index)
            .ok()
            .and_then(|index| audio_bus_width::<P>(self.selected(), is_input, index))
        else {
            return kInvalidArgument;
        };
        if let Some(aux) = aux_bus::<P>(index as usize).filter(|_| !is_input) {
            let aux = P::AUX_OUTPUTS[aux];
            bus.mediaType = media;
            bus.direction = direction;
            bus.channelCount = width as i32;
            copy_wide(aux.name, &mut bus.name);
            bus.busType = BusTypes_::kAux as vst::BusType;
            bus.flags = if aux.active_by_default {
                default_active
            } else {
                0
            };
            return kResultOk;
        }
        // The main bus is the first one, except that an instrument's only
        // input is its sidechain.
        let is_main = index == 0 && !(is_input && P::LAYOUT.inputs == 0);
        bus.mediaType = media;
        bus.direction = direction;
        bus.channelCount = width as i32;
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
        state: TBool,
    ) -> tresult {
        // SAFETY: only reads constants of the plugin type.
        let count = unsafe { self.getBusCount(media, direction) };
        if !(0..count).contains(&index) {
            return kInvalidArgument;
        }
        // The sidechain's switch decides, at the next activation, whether
        // the plugin runs with it (see `running_layout`). It is called
        // while the component is inactive.
        let is_sidechain = media == MediaTypes_::kAudio as vst::MediaType
            && direction == BusDirections_::kInput as vst::BusDirection
            && sidechain_bus::<P>() == Some(index as usize);
        if is_sidechain && !self.active.load(Ordering::Relaxed) {
            self.sidechain_active.store(state != 0, Ordering::Relaxed);
        }
        // So does an auxiliary output's, whether the plugin is given it.
        let is_output = media == MediaTypes_::kAudio as vst::MediaType
            && direction == BusDirections_::kOutput as vst::BusDirection;
        if let Some(aux) = aux_bus::<P>(index as usize).filter(|_| is_output) {
            if !self.active.load(Ordering::Relaxed) {
                self.aux_active[aux].store(state != 0, Ordering::Relaxed);
            }
        }
        kResultOk
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        guard(kResultFalse, || {
            // SAFETY: `setActive` runs on the main thread while the
            // component is not processing, so nothing else is using the
            // audio state.
            let audio = unsafe { &mut *self.audio.get() };
            if state != 0 {
                let max_block = audio.max_block.max(1);
                let layout = self.running_layout();
                audio.layout = layout;
                audio.buffer = AudioBuffer::new(layout.outputs, max_block);
                audio.sidechain = AudioBuffer::new(layout.sidechain, max_block);
                audio.aux = AuxOutputs::for_plugin::<P>(max_block);
                for (index, on) in self.aux_active.iter().enumerate() {
                    audio.aux.set_active(index, on.load(Ordering::Relaxed));
                }
                audio.events = EventBuffer::with_capacity(EVENTS_PER_BLOCK);
                audio.held = EventBuffer::with_capacity(HELD_EVENTS);
                audio.raw = EventBuffer::with_capacity(EVENTS_PER_BLOCK);
                audio.plugin.set_layout(layout);
                audio.plugin.prepare(&ProcessSpec {
                    sample_rate: audio.sample_rate,
                    max_block_size: max_block,
                    num_channels: layout.outputs,
                });
                audio.prepared = true;
                self.active.store(true, Ordering::Relaxed);
                let latency = u32::try_from(audio.plugin.latency_samples()).unwrap_or(u32::MAX);
                self.latency.store(latency, Ordering::Relaxed);
            } else {
                self.active.store(false, Ordering::Relaxed);
                self.controls.running.store(false, Ordering::Relaxed);
                if audio.prepared {
                    audio.plugin.reset();
                    audio.held.clear();
                    audio.input.reset();
                    audio.notes.reset();
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
        // Accepted when it is the arrangement of a layout the plugin
        // offers, bus for bus, which the plugin then runs with from the
        // next activation; a sidechain left out is an empty arrangement.
        // A host that asks for anything else is told no, and reads back
        // with `getBusArrangement` what is kept.
        let matches = |layout: AudioLayout| {
            [(inputs, num_inputs, true), (outputs, num_outputs, false)]
                .into_iter()
                .all(|(wanted, count, is_input)| {
                    // The auxiliary outputs have one width each, which has
                    // to be asked for as it is.
                    let ours = audio_bus_count::<P>(is_input);
                    if count < 0 || count as usize != ours || (ours > 0 && wanted.is_null()) {
                        return false;
                    }
                    (0..ours).all(|index| {
                        let width = audio_bus_width::<P>(layout, is_input, index).unwrap_or(0);
                        // SAFETY: the host passes `count` arrangements, and
                        // `index` is below `count`.
                        unsafe { *wanted.add(index) == arrangement(width) }
                    })
                })
        };
        if self.active.load(Ordering::Relaxed) {
            // Called while active, which the specification does not
            // allow: nothing changes, and only what is in use is accepted.
            return if matches(self.selected()) {
                kResultOk
            } else {
                kResultFalse
            };
        }
        match self.layouts.iter().position(|&layout| matches(layout)) {
            Some(index) => {
                self.config.store(index, Ordering::Relaxed);
                kResultOk
            }
            None => kResultFalse,
        }
    }

    unsafe fn getBusArrangement(
        &self,
        direction: vst::BusDirection,
        index: i32,
        arr: *mut vst::SpeakerArrangement,
    ) -> tresult {
        let is_input = direction == BusDirections_::kInput as vst::BusDirection;
        let width = usize::try_from(index)
            .ok()
            .and_then(|index| audio_bus_width::<P>(self.selected(), is_input, index));
        let (false, Some(width)) = (arr.is_null(), width) else {
            return kInvalidArgument;
        };
        // SAFETY: a non-null pointer to a value the host owns.
        unsafe { *arr = arrangement(width) };
        kResultOk
    }

    unsafe fn canProcessSampleSize(&self, size: i32) -> tresult {
        // 64-bit buffers are read rounded to 32 bits and written widened
        // (see `read_bus`), so the plugin's one 32-bit path serves both.
        if size == vst::SymbolicSampleSizes_::kSample32 as i32
            || size == vst::SymbolicSampleSizes_::kSample64 as i32
        {
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
        // SAFETY: only compares the size with the two this answers for.
        if unsafe { self.canProcessSampleSize(setup.symbolicSampleSize) } != kResultOk {
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
            audio.aux.start_block(frames);
            let layout = audio.layout;
            let wide = data.symbolicSampleSize == vst::SymbolicSampleSizes_::kSample64 as i32;
            // SAFETY: the bus pointers in `data` are valid for this call,
            // in the sample size it gives.
            unsafe {
                let inputs = buses(data.inputs, data.numInputs);
                if layout.inputs > 0 {
                    if let Some(main) = inputs.first() {
                        read_bus(main, &mut audio.buffer, layout.inputs, wide);
                    }
                }
                if let (true, Some(index)) = (layout.sidechain > 0, sidechain_bus::<P>()) {
                    audio.sidechain.set_num_frames(frames);
                    audio.sidechain.clear();
                    if let Some(bus) = inputs.get(index) {
                        read_bus(bus, &mut audio.sidechain, layout.sidechain, wide);
                    }
                }
            }

            // SAFETY: the context pointer is valid for this call, or null.
            let transport = unsafe { data.processContext.as_ref() }
                .map_or(Transport::STOPPED, convert_transport);
            let mut context = ProcessContext::new(&audio.events, &transport);
            if layout.sidechain > 0 {
                context = context.with_sidechain(&audio.sidechain);
            }
            let (plugin, buffer, output, aux) = (
                &mut audio.plugin,
                &mut audio.buffer,
                &mut audio.notes.output,
                &mut audio.aux,
            );
            output.clear();
            let finished = catch_unwind(AssertUnwindSafe(|| {
                plugin.process_with_aux(buffer, &context, output, aux);
            }))
            .is_ok();
            if !finished {
                // The plugin panicked. The host is told the block failed,
                // and the next block with audio starts with every
                // parameter's value, since the changes of this one may not
                // have reached the plugin.
                audio.resend_values = true;
                return kResultFalse;
            }

            // SAFETY: the bus pointers in `data` are valid for this call.
            // Everything was read from the inputs before this point, so
            // writing is sound even when the host processes in place.
            unsafe {
                // The main output, then the auxiliary ones; a bus switched
                // off is silent.
                for (index, bus) in buses(data.outputs, data.numOutputs).iter_mut().enumerate() {
                    let source = match aux_bus::<P>(index) {
                        Some(aux) => audio.aux.bus(aux),
                        None => (index == 0).then_some(&audio.buffer),
                    };
                    write_bus(bus, source, frames, wide);
                }
                audio.notes.send(frames, data.outputEvents);
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
        let Some((channel, controller)) = u32::try_from(index - self.controls.ids.len())
            .ok()
            .and_then(Self::midi_param_at)
        else {
            return kInvalidArgument;
        };
        let id = Self::midi_param_id(channel, controller);
        let is_bend = controller == MidiController::PITCH_BEND;
        info.id = id;
        // Built without allocating: a host may ask for many of these.
        let shown_channel = Self::MIDI.is_per_channel().then_some(channel + 1);
        let title = midi_title(shown_channel, controller);
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
            None => match Self::midi_param(id) {
                Some((_, MidiController::PITCH_BEND)) => 0.5,
                _ => 0.0,
            },
        }
    }

    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        if value.is_nan() {
            return kInvalidArgument;
        }
        let Some(index) = self.index_of(id) else {
            // Controller moves reach the plugin through `process`.
            return if Self::midi_param(id).is_some() {
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

/// A title for a hidden controller parameter: `"MIDI 3 CC 74"` for one
/// channel's, numbered from 1, or `"MIDI CC 74"` for one shared by all.
fn midi_title(channel: Option<u32>, controller: MidiController) -> FixedText {
    let mut text = FixedText {
        bytes: [0; 24],
        len: 0,
    };
    // The longest title is 18 bytes, so writing cannot fail.
    let _ = text.write_str("MIDI ");
    if let Some(channel) = channel {
        let _ = write!(text, "{channel} ");
    }
    let _ = match controller.control_number() {
        Some(number) => write!(text, "CC {number}"),
        None if controller == MidiController::PITCH_BEND => text.write_str("Pitch Bend"),
        None => text.write_str("Pressure"),
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
        let (Ok(channel), Ok(controller)) = (u32::try_from(channel), usize::try_from(controller))
        else {
            return kResultFalse;
        };
        let Some(controller) = MidiController::from_index(controller) else {
            return kResultFalse;
        };
        if bus != 0 || id.is_null() || channel >= MIDI_CHANNELS || !Self::MIDI.contains(controller)
        {
            return kResultFalse;
        }
        // Every channel shares channel 0's parameter unless the plugin
        // asked for channels; `push_controller` then delivers its moves on
        // every channel.
        let own = if Self::MIDI.is_per_channel() {
            channel
        } else {
            0
        };
        // SAFETY: a non-null pointer to a value the host owns.
        unsafe { *id = Self::midi_param_id(own, controller) };
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
