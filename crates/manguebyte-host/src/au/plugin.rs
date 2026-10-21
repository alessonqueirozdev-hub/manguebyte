//! A hosted Audio Unit, as the thread that created it deals with it.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

use manguebyte_core::ParamId;

use super::channels::{AuBus, MAX_CHANNELS, float_format, read_buses, supported};
use super::component::AuComponentInfo;
use super::events::ParamSlot;
use super::library::AuLibrary;
use super::params::{
    AuParam, plain_text, plain_value, read_params, text_from_unit, value_from_unit,
};
use super::processor::{AuProcessor, Setup};
use super::sys::*;
use super::transport::HostTransport;
use super::unit::Unit;
use super::{AuError, MAX_BLOCK_SIZE, MAX_LATENCY_FRAMES, refused, state};

/// What the two halves share. The unit is put away first, before the
/// transport its callbacks read.
pub(crate) struct Shared {
    pub(crate) unit: Unit,
    pub(crate) transport: Box<HostTransport>,
    pub(crate) processor_alive: AtomicBool,
    pub(crate) latency: AtomicU32,
    pub(crate) failed_blocks: AtomicU64,
    pub(crate) lost_events: AtomicU64,
    pub(crate) undeliverable_events: AtomicU64,
    /// Set when a block was silent because of the sample rate.
    pub(crate) wrong_rate: AtomicBool,
    /// The status of the last render that failed.
    pub(crate) last_error: AtomicI32,
}

/// Why a block came out silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuFailure {
    /// `AudioUnitRender` returned this status.
    Render(i32),
    /// The processor was prepared for a sample rate other than the one the
    /// unit was activated with. Drop it and activate the unit again.
    SampleRateMismatch,
}

/// Something to know about a unit, as returned by [`AuPlugin::pump`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AuEvent {
    /// The unit's latency is now this many frames.
    LatencyChanged(u32),
    /// At least one block came out silent, for this reason, since the last
    /// call.
    ProcessFailed(AuFailure),
    /// Something the host noticed about the unit.
    Remark(String),
}

/// A hosted Audio Unit, as the thread that created it deals with it: its
/// parameters, its channels, its state, and activation, which hands out
/// the [`AuProcessor`] that renders it.
///
/// Like the CLAP and VST 3 plugins of this crate, it stays on the thread
/// that created it, which Apple's hosts treat as the unit's main thread;
/// its processor can be sent to the audio thread.
///
/// ```compile_fail
/// fn can_be_sent<T: Send>() {}
/// can_be_sent::<manguebyte_host::au::AuPlugin>();
/// ```
///
/// ```
/// fn can_be_sent<T: Send>() {}
/// can_be_sent::<manguebyte_host::au::AuProcessor>();
/// ```
///
/// Dropping the plugin while its processor exists leaves the instance to
/// the processor, which puts it away when it is dropped in turn.
pub struct AuPlugin {
    _library: AuLibrary,
    info: AuComponentInfo,
    shared: Arc<Shared>,
    params: Vec<AuParam>,
    slots: Arc<[ParamSlot]>,
    inputs: Vec<AuBus>,
    outputs: Vec<AuBus>,
    supported: Vec<AUChannelInfo>,
    has_bypass: bool,
    /// The sample rate of the last activation, for latency and tail.
    sample_rate: f64,
    reported_latency: u32,
    reported_failures: u64,
    waiting: Vec<AuEvent>,
    _not_send: PhantomData<*const ()>,
}

impl AuPlugin {
    pub(crate) fn new(
        library: AuLibrary,
        info: AuComponentInfo,
        unit: Unit,
    ) -> Result<Self, AuError> {
        let shared = Arc::new(Shared {
            unit,
            transport: Box::new(HostTransport::new()),
            processor_alive: AtomicBool::new(false),
            latency: AtomicU32::new(0),
            failed_blocks: AtomicU64::new(0),
            lost_events: AtomicU64::new(0),
            undeliverable_events: AtomicU64::new(0),
            wrong_rate: AtomicBool::new(false),
            last_error: AtomicI32::new(noErr),
        });
        let unit = &shared.unit;
        let mut waiting = Vec::new();
        // The callbacks read the transport the processor writes; a unit
        // that does not take them is told nothing about the music.
        let callbacks = shared.transport.callbacks();
        if unit
            .set(
                kAudioUnitProperty_HostCallbacks,
                kAudioUnitScope_Global,
                0,
                &callbacks,
            )
            .is_err()
        {
            waiting.push(AuEvent::Remark(
                "the unit does not take the host's transport callbacks".into(),
            ));
        }
        let (params, skipped) = read_params(unit);
        if skipped > 0 {
            waiting.push(AuEvent::Remark(format!(
                "{skipped} parameter(s) were left out: the unit could not describe them, or \
                 listed them twice"
            )));
        }
        let mut slots: Vec<ParamSlot> = params
            .iter()
            .map(|param| ParamSlot {
                id: param.id.0,
                min: param.min,
                max: param.max,
                stepped: param.steps.is_some(),
            })
            .collect();
        slots.sort_by_key(|slot| slot.id);
        let (inputs, outputs) = read_buses(unit);
        let supported = unit
            .get_list(
                kAudioUnitProperty_SupportedNumChannels,
                kAudioUnitScope_Global,
                0,
                256,
            )
            .unwrap_or_default();
        let has_bypass = unit
            .property_info(kAudioUnitProperty_BypassEffect, kAudioUnitScope_Global, 0)
            .is_ok();
        let sample_rate = unit
            .get::<AudioStreamBasicDescription>(
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Output,
                0,
            )
            .map(|format| format.mSampleRate)
            .ok()
            .filter(|rate| rate.is_finite() && *rate > 0.0)
            .unwrap_or(44_100.0);
        let mut plugin = Self {
            _library: library,
            info,
            shared,
            params,
            slots: slots.into(),
            inputs,
            outputs,
            supported,
            has_bypass,
            sample_rate,
            reported_latency: 0,
            reported_failures: 0,
            waiting,
            _not_send: PhantomData,
        };
        plugin.reported_latency = plugin.read_latency();
        Ok(plugin)
    }

    fn unit(&self) -> &Unit {
        &self.shared.unit
    }

    /// What the library says about the component.
    pub fn info(&self) -> &AuComponentInfo {
        &self.info
    }

    // --- Parameters --------------------------------------------------------------

    /// The unit's parameters, in the order it lists them.
    pub fn params(&self) -> &[AuParam] {
        &self.params
    }

    /// The parameter with this id, if the unit has one.
    pub fn param(&self, id: ParamId) -> Option<&AuParam> {
        self.params.iter().find(|param| param.id == id)
    }

    /// The current value of a parameter, in its own unit, as
    /// `AudioUnitGetParameter` gives it: while a processor exists, what
    /// the unit last took, so a value set with
    /// [`set_param`](Self::set_param) shows once a block has been
    /// processed, or at once, as the unit has it.
    pub fn param_value(&self, id: ParamId) -> Option<f64> {
        self.param(id)?;
        self.unit().parameter(id.0).ok().map(f64::from)
    }

    /// Sets a parameter, in its own unit, held to its range, through
    /// `AudioUnitSetParameter`. While a processor exists, the unit hears it
    /// from the first frame of the next slice it renders.
    ///
    /// # Errors
    ///
    /// [`AuError::UnknownParam`], [`AuError::InvalidArgument`] for a value
    /// that is not a number, and [`AuError::Status`] if the unit refuses.
    pub fn set_param(&mut self, id: ParamId, value: f64) -> Result<(), AuError> {
        let param = self.param(id).ok_or(AuError::UnknownParam(id))?;
        if value.is_nan() {
            return Err(AuError::InvalidArgument(
                "a parameter value has to be a number",
            ));
        }
        let held = param.hold(value) as f32;
        self.unit()
            .set_parameter(id.0, held)
            .map_err(refused("AudioUnitSetParameter"))
    }

    /// The text a person should see for `value` of a parameter: the unit's
    /// own, for a unit that words its values, or the number with its
    /// unit's symbol.
    pub fn param_to_text(&self, id: ParamId, value: f64) -> Option<String> {
        let param = self.param(id)?;
        if value.is_nan() {
            return None;
        }
        let held = param.hold(value);
        if param.has_strings() {
            if let Some(text) = text_from_unit(self.unit(), id.0, held as f32) {
                return Some(text);
            }
        }
        Some(plain_text(param, held))
    }

    /// The value a piece of text stands for: as the unit reads it, for one
    /// that words its values, or as a number, with or without the unit's
    /// symbol. Held to the range.
    pub fn text_to_param(&self, id: ParamId, text: &str) -> Option<f64> {
        let param = self.param(id)?;
        if param.has_strings() {
            if let Some(value) = value_from_unit(self.unit(), id.0, text.trim()) {
                return Some(param.hold(f64::from(value)));
            }
        }
        plain_value(param, text)
    }

    // --- Channels ----------------------------------------------------------------

    /// The unit's input elements: the main input first, then the sidechain,
    /// each if it has it.
    pub fn audio_inputs(&self) -> &[AuBus] {
        &self.inputs
    }

    /// The unit's output element.
    pub fn audio_outputs(&self) -> &[AuBus] {
        &self.outputs
    }

    /// What the unit says it takes, as `kAudioUnitProperty_SupportedNumChannels`
    /// lists it: pairs of main input and output channel counts, where -1
    /// and -2 stand for any number and a number below -2 for up to that
    /// many. Empty for a unit that does not say.
    pub fn supported_channels(&self) -> Vec<(i16, i16)> {
        self.supported
            .iter()
            .map(|info| (info.inChannels, info.outChannels))
            .collect()
    }

    /// Sets how many channels the main input and the output have, by
    /// setting their stream formats. The sidechain keeps its own.
    ///
    /// # Errors
    ///
    /// [`AuError::ProcessorAlive`] while a processor exists,
    /// [`AuError::Channels`] for counts the unit does not list, or for
    /// input channels on a unit that has no input, and [`AuError::Status`]
    /// if the unit refuses the format.
    pub fn set_channels(&mut self, inputs: u32, outputs: u32) -> Result<(), AuError> {
        if self.is_active() {
            return Err(AuError::ProcessorAlive);
        }
        let refused_counts = AuError::Channels { inputs, outputs };
        let has_input = self.inputs.first().is_some_and(|bus| bus.element == 0);
        if outputs == 0
            || outputs > MAX_CHANNELS
            || inputs > MAX_CHANNELS
            || (inputs > 0 && !has_input)
            || !supported(&self.supported, inputs, outputs)
        {
            return Err(refused_counts);
        }
        let unit = &self.shared.unit;
        if has_input {
            unit.set(
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                0,
                &float_format(self.sample_rate, inputs),
            )
            .map_err(|_| refused_counts.clone())?;
        }
        unit.set(
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            0,
            &float_format(self.sample_rate, outputs),
        )
        .map_err(|_| refused_counts)?;
        let (inputs, outputs) = read_buses(unit);
        self.inputs = inputs;
        self.outputs = outputs;
        Ok(())
    }

    /// Whether the unit takes MIDI: it is a music device, a music effect or
    /// a MIDI processor.
    pub fn takes_notes(&self) -> bool {
        self.shared.unit.takes_midi()
    }

    // --- Latency, tail, bypass -------------------------------------------------

    fn read_latency(&self) -> u32 {
        let seconds = self
            .unit()
            .get::<f64>(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0)
            .unwrap_or(0.0);
        seconds_to_frames(seconds, self.sample_rate).min(MAX_LATENCY_FRAMES)
    }

    /// How many frames late the unit's output is, as it said at its last
    /// activation or the last [`pump`](Self::pump). At most
    /// [`MAX_LATENCY_FRAMES`].
    pub fn latency(&self) -> u32 {
        self.shared.latency.load(Ordering::Relaxed)
    }

    /// How long the unit goes on sounding after its input stops, in
    /// seconds, as `kAudioUnitProperty_TailTime` says; 0 for a unit that
    /// does not say.
    pub fn tail_seconds(&self) -> f64 {
        self.unit()
            .get::<f64>(kAudioUnitProperty_TailTime, kAudioUnitScope_Global, 0)
            .ok()
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .unwrap_or(0.0)
    }

    /// Whether the unit can be bypassed (`kAudioUnitProperty_BypassEffect`).
    pub fn can_bypass(&self) -> bool {
        self.has_bypass
    }

    /// Whether the unit is bypassed.
    pub fn is_bypassed(&self) -> bool {
        self.has_bypass
            && self
                .unit()
                .get::<u32>(kAudioUnitProperty_BypassEffect, kAudioUnitScope_Global, 0)
                .is_ok_and(|on| on != 0)
    }

    /// Bypasses the unit, which then passes its input through, or brings it
    /// back. Set from this thread, as properties are; a processor hears it
    /// from its next slice.
    ///
    /// # Errors
    ///
    /// [`AuError::Unsupported`] for a unit without the property, and
    /// [`AuError::Status`] if it refuses.
    pub fn set_bypass(&mut self, bypass: bool) -> Result<(), AuError> {
        if !self.has_bypass {
            return Err(AuError::Unsupported("this unit cannot be bypassed"));
        }
        self.unit()
            .set(
                kAudioUnitProperty_BypassEffect,
                kAudioUnitScope_Global,
                0,
                &u32::from(bypass),
            )
            .map_err(refused("setting BypassEffect"))
    }

    // --- State ------------------------------------------------------------------

    /// Everything the unit needs to come back as it is now: the property
    /// list of `kAudioUnitProperty_ClassInfo`, in its XML form, which is
    /// what an `.aupreset` file holds.
    ///
    /// On macOS CoreFoundation writes and reads the property list
    /// (`CFPropertyListCreateData`, `CFPropertyListCreateWithData`), so
    /// every key a unit puts in its dictionary goes out and comes back.
    /// Elsewhere, where a unit can only be hosted in process from its
    /// factory and its objects are the AU wrapper's CoreFoundation
    /// stand-in, the keys Apple's base classes write (`version`, `type`,
    /// `subtype`, `manufacturer`, `name` and `data`) are kept, by this
    /// crate's own property list code, in the same form.
    ///
    /// # Errors
    ///
    /// [`AuError::Status`] if the unit gives none,
    /// [`AuError::StateSaveFailed`] if what it gives cannot be written, and
    /// [`AuError::StateTooLarge`] past [`MAX_STATE_BYTES`](super::MAX_STATE_BYTES).
    pub fn save_state(&mut self) -> Result<Vec<u8>, AuError> {
        state::save(self.unit())
    }

    /// Gives the unit back a state saved by [`save_state`](Self::save_state).
    /// Apple's units take one at any time, so this works while a processor
    /// exists; the unit hears it from its next slice.
    ///
    /// # Errors
    ///
    /// [`AuError::BadState`] for bytes that are not such a state,
    /// [`AuError::StateTooLarge`], and [`AuError::Status`] if the unit
    /// refuses it, as it does another unit's.
    pub fn load_state(&mut self, bytes: &[u8]) -> Result<(), AuError> {
        state::load(self.unit(), bytes)
    }

    // --- Activation ----------------------------------------------------------

    /// Whether a processor exists: the unit is initialised for it.
    pub fn is_active(&self) -> bool {
        self.shared.processor_alive.load(Ordering::Acquire)
    }

    /// The same as [`is_active`](Self::is_active): the unit is initialised
    /// for as long as its processor exists.
    pub fn has_processor(&self) -> bool {
        self.is_active()
    }

    /// Activates the unit for `sample_rate`, in blocks of up to
    /// `max_block_size` frames, and returns the processor that renders it.
    ///
    /// Every element is given the stream format at `sample_rate`, the unit
    /// is told the longest slice (`MaximumFramesPerSlice`), a render
    /// callback feeds each input element, and the unit is initialised. A
    /// unit that refuses the slice length keeps its own, and longer blocks
    /// are rendered in slices of that. Everything processing needs is
    /// allocated here.
    ///
    /// # Errors
    ///
    /// [`AuError::AlreadyActive`] while a processor exists,
    /// [`AuError::InvalidArgument`] for a sample rate that is not a
    /// positive number or a block size of zero or above
    /// [`MAX_BLOCK_SIZE`], and [`AuError::Status`] if the unit refuses a
    /// format or its initialisation.
    pub fn activate(
        &mut self,
        sample_rate: f64,
        max_block_size: usize,
    ) -> Result<AuProcessor, AuError> {
        if self.is_active() {
            return Err(AuError::AlreadyActive);
        }
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            return Err(AuError::InvalidArgument(
                "the sample rate has to be a positive number",
            ));
        }
        if max_block_size == 0 || max_block_size > MAX_BLOCK_SIZE {
            return Err(AuError::InvalidArgument(
                "the block size has to be between 1 and MAX_BLOCK_SIZE",
            ));
        }
        let unit = &self.shared.unit;
        // A unit is configured while it is not initialised.
        let _ = unit.uninitialize();
        for bus in &self.inputs {
            unit.set(
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                bus.element,
                &float_format(sample_rate, bus.channels),
            )
            .map_err(refused("an input's stream format"))?;
        }
        for bus in &self.outputs {
            unit.set(
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Output,
                bus.element,
                &float_format(sample_rate, bus.channels),
            )
            .map_err(refused("the output's stream format"))?;
        }
        let wanted = u32::try_from(max_block_size).unwrap_or(u32::MAX);
        let slice = match unit.set(
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
            &wanted,
        ) {
            Ok(()) => wanted,
            Err(_) => {
                let own = unit
                    .get::<u32>(
                        kAudioUnitProperty_MaximumFramesPerSlice,
                        kAudioUnitScope_Global,
                        0,
                    )
                    .unwrap_or(0);
                if own == 0 {
                    return Err(AuError::Unsupported("the unit takes no length of slice"));
                }
                self.waiting.push(AuEvent::Remark(format!(
                    "the unit refused slices of {wanted} frames, and renders {own} at most"
                )));
                own.min(wanted)
            }
        };
        self.sample_rate = sample_rate;
        let main_input = self
            .inputs
            .iter()
            .find(|bus| bus.element == 0)
            .map_or(0, |bus| bus.channels as usize);
        let sidechain = self
            .inputs
            .iter()
            .find(|bus| bus.element == 1)
            .map_or(0, |bus| bus.channels as usize);
        let outputs = self.outputs.first().map_or(0, |bus| bus.channels as usize);
        self.shared.wrong_rate.store(false, Ordering::Relaxed);
        let processor = AuProcessor::new(Setup {
            shared: Arc::clone(&self.shared),
            slots: Arc::clone(&self.slots),
            sample_rate,
            slice: slice as usize,
            inputs: [
                (self.inputs.iter().any(|bus| bus.element == 0), main_input),
                (self.inputs.iter().any(|bus| bus.element == 1), sidechain),
            ],
            outputs,
        })?;
        let latency = self.read_latency();
        self.shared.latency.store(latency, Ordering::Relaxed);
        if latency != self.reported_latency {
            self.reported_latency = latency;
            self.waiting.push(AuEvent::LatencyChanged(latency));
        }
        Ok(processor)
    }

    /// Does nothing once the processor has been dropped, which uninitialises
    /// the unit. Kept so that a plugin of any format is put away the same
    /// way.
    ///
    /// # Errors
    ///
    /// [`AuError::ProcessorAlive`] if the processor still exists.
    pub fn deactivate(&mut self) -> Result<(), AuError> {
        if self.is_active() {
            Err(AuError::ProcessorAlive)
        } else {
            Ok(())
        }
    }

    // --- What happened ---------------------------------------------------------

    /// How many blocks have come out silent, since the unit was created.
    pub fn failed_blocks(&self) -> u64 {
        self.shared.failed_blocks.load(Ordering::Relaxed)
    }

    /// How many events have been dropped for want of room in the list that
    /// carries events to the next block.
    pub fn lost_events(&self) -> u64 {
        self.shared.lost_events.load(Ordering::Relaxed)
    }

    /// How many events have not been sent because the unit has no way to
    /// receive them: notes for a unit that takes no MIDI, values for
    /// parameters it does not have, what MIDI 1.0 cannot carry (note
    /// expression other than pressure, chokes, modulation), and events the
    /// unit refused.
    pub fn undeliverable_events(&self) -> u64 {
        self.shared.undeliverable_events.load(Ordering::Relaxed)
    }

    /// What there is to know since the last call: a new latency, silent
    /// blocks, and the host's own remarks. The latency is read from the
    /// unit here, since few units say when it changes.
    pub fn pump(&mut self) -> Vec<AuEvent> {
        let latency = self.read_latency();
        if latency != self.reported_latency {
            self.reported_latency = latency;
            self.shared.latency.store(latency, Ordering::Relaxed);
            self.waiting.push(AuEvent::LatencyChanged(latency));
        }
        if self.shared.wrong_rate.swap(false, Ordering::Relaxed) {
            self.waiting
                .push(AuEvent::ProcessFailed(AuFailure::SampleRateMismatch));
        }
        let failed = self.shared.failed_blocks.load(Ordering::Relaxed);
        let status = self.shared.last_error.swap(noErr, Ordering::Relaxed);
        if failed != self.reported_failures {
            self.reported_failures = failed;
            if status != noErr {
                self.waiting
                    .push(AuEvent::ProcessFailed(AuFailure::Render(status)));
            }
        }
        std::mem::take(&mut self.waiting)
    }
}

impl std::fmt::Debug for AuPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuPlugin")
            .field("codes", &self.info.codes.to_string())
            .field("name", &self.info.name)
            .field("active", &self.is_active())
            .field("params", &self.params.len())
            .finish_non_exhaustive()
    }
}

/// A time in seconds as whole frames at `sample_rate`.
pub(crate) fn seconds_to_frames(seconds: f64, sample_rate: f64) -> u32 {
    let frames = seconds * sample_rate;
    if frames.is_finite() && frames > 0.0 {
        frames.round().min(f64::from(u32::MAX)) as u32
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_become_frames() {
        assert_eq!(seconds_to_frames(0.01, 48_000.0), 480);
        assert_eq!(seconds_to_frames(64.0 / 44_100.0, 44_100.0), 64);
        for nothing in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(seconds_to_frames(nothing, 48_000.0), 0);
        }
    }
}
