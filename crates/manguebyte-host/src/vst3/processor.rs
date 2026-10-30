//! The audio-thread half of a hosted VST 3 plugin.

// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast that is needed on one platform is redundant on the
// other.
#![allow(clippy::unnecessary_cast)]

use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use manguebyte_core::spsc::{Consumer, Producer};
use manguebyte_core::{AudioBuffer, NoDenormals, ParamId};
use manguebyte_dsp::event::{Event, EventKind};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};
use vst3::ComWrapper;
use vst3::Steinberg::Vst::{self as vst, IAudioProcessor, IAudioProcessorTrait};
use vst3::Steinberg::{kNotImplemented, kResultOk};

use super::changes::{EventList, ParamChanges, Point, TimedEvent, changes_ptr, events_ptr};
use super::convert::{MidiMap, Outgoing, advance, incoming, outgoing, process_context, unit};
use super::plugin::{Tail, tail_of};
use super::shared::{Delivered, Shared};
use super::{
    CARRIED_EVENTS, EVENTS_PER_BLOCK, FromAudio, OUTPUT_POINTS_PER_QUEUE, OUTPUT_QUEUES,
    POINTS_PER_BLOCK, ProcessFailure, QUEUES_PER_BLOCK, ToAudio,
};

/// How the last block went.
///
/// More states may be added, so a `match` on this needs a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProcessStatus {
    /// No block has been processed yet.
    Idle,
    /// The plugin processed the block.
    Processed,
    /// The block is silent because the plugin failed.
    Failed(ProcessFailure),
}

/// Everything [`Vst3Processor::new`] needs.
pub(crate) struct Setup {
    pub(crate) shared: Arc<Shared>,
    pub(crate) processor: *mut IAudioProcessor,
    pub(crate) sample_rate: f64,
    pub(crate) max_frames: usize,
    /// Channels of each audio input bus, in the plugin's order.
    pub(crate) inputs: Vec<u32>,
    /// Channels of each audio output bus.
    pub(crate) outputs: Vec<u32>,
    pub(crate) main_input: Option<usize>,
    pub(crate) sidechain: Option<usize>,
    pub(crate) has_event_input: bool,
    pub(crate) has_event_output: bool,
    pub(crate) delivered: Arc<Delivered>,
    pub(crate) midi_map: Option<Box<MidiMap>>,
    pub(crate) from_main: Consumer<ToAudio>,
    pub(crate) to_main: Producer<FromAudio>,
}

/// A hosted VST 3 plugin as the audio thread deals with it.
///
/// It is a [`Processor`]: put it in a device callback, a chain or a
/// `manguebyte-graph` graph like any other. It is made by
/// [`Vst3Plugin::activate`](super::Vst3Plugin::activate), and is the only
/// way to process audio with the plugin.
///
/// # Real-time safety
///
/// [`process`](Processor::process), [`process_with`](Processor::process_with),
/// [`reset`](Processor::reset) and [`flush`](Self::flush) do not allocate,
/// lock or block in this crate's code: buffers, bus tables, parameter
/// changes, event lists and the controller map are all allocated at
/// activation. What the plugin does inside its own functions is up to the
/// plugin. Subnormal numbers are flushed to zero while the plugin runs.
///
/// Dropping the processor is *not* real-time safe: it tells the plugin to
/// stop processing and frees its buffers. Drop it away from the audio
/// thread; a graph does that by itself.
///
/// # How a block is laid out
///
/// The buffer passed to `process_with` is the plugin's main input on the
/// way in and its main output on the way out. Channels the plugin does not
/// have are silent, and channels the buffer does not have are discarded.
/// [`ProcessContext::sidechain`] feeds the plugin's first auxiliary input.
/// A sidechain with fewer frames than the block is treated as not connected
/// for the whole of that block. Further buses get silence and are
/// discarded. A block longer than the size the plugin was activated with is
/// processed in pieces of that size, with every event still on its frame.
///
/// An event placed at or beyond the end of the block is kept and delivered
/// on the first frame of the next block.
///
/// # Events
///
/// - A parameter value is a *normalized* value, `0..=1`, as VST 3
///   automates it, and is held to that range. Every value of a block
///   becomes a point in that parameter's queue, on its frame, in order.
/// - A note keeps its channel, key and id. Pressure for one note is a
///   polyphonic pressure event; the other note expressions are note
///   expression events, in VST 3's scales, for notes that have an id.
/// - A MIDI controller, pitch bend or channel pressure is a point for the
///   parameter the plugin maps it to.
///
/// An event the plugin has no way to receive is not sent, and is counted in
/// [`Vst3Plugin::undeliverable_events`](super::Vst3Plugin::undeliverable_events).
/// At most [`EVENTS_PER_BLOCK`] events and
/// [`POINTS_PER_BLOCK`](super::POINTS_PER_BLOCK) points go to the plugin in
/// one call; more are dropped and counted in
/// [`Vst3Plugin::lost_events`](super::Vst3Plugin::lost_events).
///
/// What the plugin reports in return, notes and controller moves from its
/// event list and every point of its output parameter changes, is in
/// [`output_events`](Self::output_events) after each call.
///
/// # When the plugin fails
///
/// The block is silent, [`status`](Self::status) says why, and the main
/// thread sees the same from [`Vst3Plugin::pump`](super::Vst3Plugin::pump).
/// The processor goes on calling the plugin on later blocks. The events of
/// a block the plugin could not be called for at all are kept, and
/// delivered on the first frame of the next block it processes.
///
/// Before its first block the plugin is told processing starts
/// (`setProcessing`). An answer of `kResultOk`, or `kNotImplemented` from a
/// plugin that has no use for the call, lets it start; any other answer is a
/// refusal, [`ProcessFailure::StartRefused`], and the plugin is asked again
/// on the next block.
///
/// ```
/// use manguebyte_dsp::Processor;
/// use manguebyte_host::vst3::Vst3Processor;
///
/// // A processor like any other, which can be sent to the thread that
/// // runs the audio.
/// fn is_a_processor<T: Processor<f32> + Send>() {}
/// is_a_processor::<Vst3Processor>();
/// ```
pub struct Vst3Processor {
    shared: Arc<Shared>,
    /// The plugin's processor. `Shared` owns a reference to it and releases
    /// it only once this value is gone.
    processor: *mut IAudioProcessor,
    sample_rate: f64,
    max_frames: usize,
    io: Io,
    changes_in: ComWrapper<ParamChanges>,
    changes_out: ComWrapper<ParamChanges>,
    events_in: ComWrapper<EventList>,
    events_out: ComWrapper<EventList>,
    has_event_input: bool,
    has_event_output: bool,
    /// The points of the piece being processed, before they are sorted into
    /// their queues.
    points: Vec<Point>,
    /// What the plugin reported during the caller's block.
    output: Vec<Event>,
    /// Events still to be delivered: the ones that fell beyond the end of
    /// the last block, and the ones of blocks the plugin was not called
    /// for.
    carry: Vec<Event>,
    delivered: Arc<Delivered>,
    midi_map: Option<Box<MidiMap>>,
    /// The main thread's queue. `None` only while being dropped.
    from_main: Option<Consumer<ToAudio>>,
    to_main: Producer<FromAudio>,
    /// The generation of parameter values of the piece being processed:
    /// what it delivers and reports is stamped with it.
    generation: u64,
    /// Frames processed since activation.
    continuous: i64,
    /// Events dropped since the count was last passed on.
    lost: u64,
    /// Events not sent since the count was last passed on, because the
    /// plugin has no way to receive them.
    undeliverable: u64,
    started: bool,
    wrong_rate: bool,
    status: ProcessStatus,
}

// SAFETY: the raw pointers in here are the plugin's processor and tables
// that point into memory this value owns. VST 3 has a plugin's `process`
// called from whichever thread a host processes on, one at a time, and
// every such call here goes through `&mut self`.
unsafe impl Send for Vst3Processor {}

impl Vst3Processor {
    /// Allocates everything processing will need.
    pub(crate) fn new(setup: Setup) -> Self {
        setup.shared.processor_created();
        Self {
            io: Io::new(
                &setup.inputs,
                &setup.outputs,
                setup.main_input,
                setup.sidechain,
                setup.max_frames,
            ),
            changes_in: ParamChanges::for_input(QUEUES_PER_BLOCK, POINTS_PER_BLOCK),
            changes_out: ParamChanges::for_output(OUTPUT_QUEUES, OUTPUT_POINTS_PER_QUEUE),
            events_in: EventList::for_input(EVENTS_PER_BLOCK),
            events_out: EventList::for_output(EVENTS_PER_BLOCK),
            has_event_input: setup.has_event_input,
            has_event_output: setup.has_event_output,
            points: Vec::with_capacity(POINTS_PER_BLOCK),
            output: Vec::with_capacity(EVENTS_PER_BLOCK),
            carry: Vec::with_capacity(CARRIED_EVENTS),
            delivered: setup.delivered,
            midi_map: setup.midi_map,
            from_main: Some(setup.from_main),
            to_main: setup.to_main,
            generation: setup.shared.generation(),
            continuous: 0,
            lost: 0,
            undeliverable: 0,
            started: false,
            wrong_rate: false,
            status: ProcessStatus::Idle,
            shared: setup.shared,
            processor: setup.processor,
            sample_rate: setup.sample_rate,
            max_frames: setup.max_frames,
        }
    }

    /// The plugin's processor, valid for as long as this value.
    fn plugin(&self) -> vst3::ComRef<'_, IAudioProcessor> {
        // SAFETY: `Shared` owns a reference to the processor and releases it
        // only once this value is gone.
        unsafe { vst3::ComRef::from_raw_unchecked(self.processor) }
    }

    /// What the plugin reported during the last call to
    /// [`process_with`](Processor::process_with) or [`flush`](Self::flush),
    /// on the frames it reported it.
    ///
    /// Notes, note expression and controller moves come from the plugin's
    /// output event list. Every point of its output parameter changes is
    /// here as a [`EventKind::ParamValue`] with a normalized value, held to
    /// `0..=1`, and also goes to the main thread, where
    /// [`Vst3Plugin::pump`](super::Vst3Plugin::pump) tells the controller.
    /// At most [`EVENTS_PER_BLOCK`] are kept for one call. Real-time safe.
    pub fn output_events(&self) -> &[Event] {
        &self.output
    }

    /// Processes one block as [`process_with`](Processor::process_with)
    /// does, and copies what the plugin wrote on its other audio output
    /// buses, those after the main one in
    /// [`Vst3Plugin::audio_outputs`](super::Vst3Plugin::audio_outputs),
    /// into `aux`: one buffer per bus, in their order.
    ///
    /// Each buffer gets as many channels as both it and its bus have, on
    /// the frames of `buffer`; anything else in it is left as it was. A
    /// bus without a buffer is not copied, and one that is switched off
    /// (see [`Vst3Plugin::set_audio_output_active`](super::Vst3Plugin::set_audio_output_active))
    /// is silent. Every buffer is silent for a block the plugin failed or
    /// was not called for. Real-time safe.
    pub fn process_with_aux(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        context: &ProcessContext<'_, f32>,
        aux: &mut [AudioBuffer<f32>],
    ) {
        // Numbers too small to hear are zero while the plugin runs, whether
        // or not the plugin thought of it.
        let _quiet = NoDenormals::new();
        self.run(buffer, context, aux);
    }

    /// How the last block went. Real-time safe.
    pub fn status(&self) -> ProcessStatus {
        self.status
    }

    /// The sample rate the plugin was activated with.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// The largest block the plugin is handed in one call. Longer blocks
    /// are processed in pieces.
    pub fn max_block_size(&self) -> usize {
        self.max_frames
    }

    /// How long the plugin goes on sounding after its input stops, as the
    /// main thread last read it: when the plugin was activated, and since
    /// then at every call of [`Vst3Plugin::pump`](super::Vst3Plugin::pump)
    /// or [`Vst3Plugin::tail`](super::Vst3Plugin::tail).
    ///
    /// VST 3 has a plugin asked for its tail on the main thread only, so
    /// this does not call the plugin. Real-time safe.
    pub fn tail(&self) -> Tail {
        tail_of(self.shared.tail())
    }

    /// Delivers the parameter changes waiting from the main thread without
    /// processing any audio, in a call of the plugin's process function
    /// with no frames, as VST 3 allows. What the plugin reports in return
    /// is in [`output_events`](Self::output_events). Real-time safe as far
    /// as this crate's code goes.
    pub fn flush(&mut self) {
        self.output.clear();
        if !self.start() {
            return;
        }
        self.prepare_piece();
        self.take_from_main();
        self.load_points();
        let _quiet = NoDenormals::new();
        let mut context = process_context(&Transport::STOPPED, self.sample_rate, self.continuous);
        self.call(0, &raw mut context);
        self.collect(0, 1);
        self.note_lost();
    }

    /// Queues one event for the plugin, at `time` frames into the piece.
    fn queue(&mut self, kind: &EventKind, time: u32) {
        match outgoing(kind, self.midi_map.as_deref()) {
            Outgoing::Note(event) if self.has_event_input => {
                let timed = TimedEvent {
                    offset: time as i32,
                    bus: 0,
                    event,
                };
                if !self.events_in.push(timed) {
                    self.lost += 1;
                }
            }
            Outgoing::Point { id, value } if self.delivered.knows(id) => {
                if self.points.len() < self.points.capacity() {
                    self.points.push(Point {
                        id,
                        order: self.points.len() as u32,
                        offset: time as i32,
                        value,
                    });
                    self.delivered.note(id, value, self.generation);
                } else {
                    self.lost += 1;
                }
            }
            _ => self.undeliverable += 1,
        }
    }

    /// Moves what the main thread sent into the piece, on frame 0, leaving
    /// out what a state loaded since has replaced.
    fn take_from_main(&mut self) {
        while let Some(message) = self.from_main.as_mut().and_then(Consumer::pop) {
            let ToAudio::ParamValue {
                id,
                value,
                generation,
            } = message;
            // Read after the message was taken, so that a state loaded
            // after it was sent is seen.
            let current = self.shared.generation();
            if generation < current {
                continue;
            }
            self.generation = current;
            let kind = EventKind::ParamValue {
                param: ParamId(id),
                value,
            };
            self.queue(&kind, 0);
        }
    }

    /// Moves the events kept from the last block into the piece, on frame 0.
    fn take_carried(&mut self) {
        let mut index = 0;
        while let Some(event) = self.carry.get(index).copied() {
            self.queue(&event.kind, 0);
            index += 1;
        }
        self.carry.clear();
    }

    /// Keeps events for the first frame of the next block the plugin
    /// processes. An event there is no room for is counted as lost.
    fn keep(&mut self, events: &[Event]) {
        for event in events {
            // Allocated with room for this many, so this does not allocate.
            if self.carry.len() < CARRIED_EVENTS {
                self.carry.push(*event);
            } else {
                self.lost += 1;
            }
        }
    }

    /// Empties what the plugin reads and writes for one call, and takes up
    /// the current generation of parameter values.
    fn prepare_piece(&mut self) {
        self.generation = self.shared.generation();
        self.points.clear();
        self.events_in.clear();
        self.changes_out.clear();
        self.events_out.clear();
    }

    /// Sorts the piece's points into one queue per parameter, each in the
    /// order its points came.
    fn load_points(&mut self) {
        // Unstable, so it does not allocate; `order` is unique, so the
        // result is the same as a stable sort's.
        self.points
            .sort_unstable_by_key(|point| (point.id, point.order));
        self.lost += self.changes_in.load(&self.points) as u64;
    }

    /// Adds the events that were dropped and the ones that could not be
    /// sent to the counts the main thread reads.
    fn note_lost(&mut self) {
        self.lost += u64::from(self.changes_out.take_refused());
        self.lost += u64::from(self.events_out.take_refused());
        if self.lost > 0 {
            self.shared
                .lost_events
                .fetch_add(self.lost, Ordering::Relaxed);
            self.lost = 0;
        }
        if self.undeliverable > 0 {
            self.shared
                .undeliverable_events
                .fetch_add(self.undeliverable, Ordering::Relaxed);
            self.undeliverable = 0;
        }
    }

    /// Records that this block is silent, and why.
    fn fail(&mut self, failure: ProcessFailure) {
        self.status = ProcessStatus::Failed(failure);
        self.shared.fail(failure);
    }

    /// Tells the plugin that processing is about to begin, the first time
    /// and after every refusal.
    fn start(&mut self) -> bool {
        if !self.started {
            // SAFETY: the plugin's active processor, which is not
            // processing; VST 3 allows this call on the audio thread.
            let result = unsafe { self.plugin().setProcessing(1) };
            // Many plugins do not implement it, which is not a refusal.
            self.started = result == kResultOk || result == kNotImplemented;
        }
        self.started
    }

    /// Ends a block the plugin cannot be called for: silence at the output,
    /// the reason on record, and the block's events kept for later.
    fn skip(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        aux: &mut [AudioBuffer<f32>],
        events: &[Event],
        failure: ProcessFailure,
    ) {
        for bus in aux.iter_mut() {
            bus.clear();
        }
        buffer.clear();
        self.keep(events);
        self.fail(failure);
        self.note_lost();
    }

    fn run(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        context: &ProcessContext<'_, f32>,
        aux: &mut [AudioBuffer<f32>],
    ) {
        self.output.clear();
        let events = context.events.as_slice();
        if self.wrong_rate {
            self.skip(buffer, aux, events, ProcessFailure::SampleRateMismatch);
            return;
        }
        if !self.start() {
            self.skip(buffer, aux, events, ProcessFailure::StartRefused);
            return;
        }

        let total = buffer.num_frames();
        // Settled once for the block, so that the answer does not depend on
        // the size the plugin was activated with.
        let sidechain = context
            .sidechain
            .filter(|sidechain| sidechain.num_frames() >= total);
        let mut next = 0;
        let mut transport = *context.transport;
        let mut failed = false;
        let mut start = 0;
        while start < total {
            let frames = (total - start).min(self.max_frames);
            let end = start + frames;

            self.prepare_piece();
            if start == 0 {
                // What did not come from the caller goes first, so that the
                // caller's own events on frame 0 come later and win.
                self.take_from_main();
                self.take_carried();
            }
            while let Some(event) = events.get(next) {
                let offset = event.offset as usize;
                if offset >= end {
                    break;
                }
                self.queue(&event.kind, offset.saturating_sub(start) as u32);
                next += 1;
            }
            self.load_points();

            self.io.load(buffer, sidechain, start, frames);
            let mut vst_context = process_context(&transport, self.sample_rate, self.continuous);
            let good = self.call(frames, &raw mut vst_context);
            self.io.store(buffer, start, frames, good);
            self.io.store_aux(aux, start, frames, good);
            self.collect(start as u32, frames as u32);
            failed |= !good;

            self.continuous = self.continuous.saturating_add(frames as i64);
            start = end;
            if start < total {
                advance(&mut transport, frames, self.sample_rate);
            }
        }

        // Whatever belongs after the last frame waits for the next block.
        self.keep(events.get(next..).unwrap_or_default());
        if failed {
            self.fail(ProcessFailure::Error);
        } else {
            self.status = ProcessStatus::Processed;
        }
        self.note_lost();
    }

    /// Calls the plugin's process function for one piece of `frames`
    /// frames. Returns whether the plugin's output is to be kept.
    fn call(&mut self, frames: usize, context: *mut vst::ProcessContext) -> bool {
        let mut data = vst::ProcessData {
            processMode: vst::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: vst::SymbolicSampleSizes_::kSample32 as i32,
            numSamples: frames as i32,
            numInputs: self.io.input_buses.len() as i32,
            numOutputs: self.io.output_buses.len() as i32,
            inputs: self.io.input_buses.as_mut_ptr(),
            outputs: self.io.output_buses.as_mut_ptr(),
            inputParameterChanges: changes_ptr(&self.changes_in),
            outputParameterChanges: changes_ptr(&self.changes_out),
            inputEvents: if self.has_event_input {
                events_ptr(&self.events_in)
            } else {
                ptr::null_mut()
            },
            outputEvents: if self.has_event_output {
                events_ptr(&self.events_out)
            } else {
                ptr::null_mut()
            },
            processContext: context,
        };
        // SAFETY: the plugin's active processor, told processing started,
        // called from the thread that holds this value. Every pointer in
        // `data` is to memory this value owns or to the caller's context;
        // the bus tables were pointed at `frames` frames of scratch memory
        // by `Io::load`, and nothing touches any of it until the call
        // returns.
        let result = unsafe { self.plugin().process(&raw mut data) };
        result == kResultOk
    }

    /// Reads what the plugin reported during one call, for a piece that
    /// starts `base` frames into the caller's block and is `frames` long.
    fn collect(&mut self, base: u32, frames: u32) {
        let last = frames.saturating_sub(1);
        // A time outside the piece is the plugin's mistake; the event is
        // kept, on the nearest frame it could have meant.
        let place = |offset: i32| base.saturating_add(u32::try_from(offset).unwrap_or(0).min(last));
        for index in 0..self.events_out.len() {
            let Some(timed) = self.events_out.get(index) else {
                continue;
            };
            let Some(kind) = incoming(timed.event) else {
                continue;
            };
            if self.output.len() < self.output.capacity() {
                self.output.push(Event {
                    offset: place(timed.offset),
                    kind,
                });
            } else {
                self.lost += 1;
            }
        }
        let output = &mut self.output;
        let to_main = &mut self.to_main;
        let lost = &mut self.lost;
        let delivered = &self.delivered;
        let generation = self.generation;
        self.changes_out.drain(|id, offset, value| {
            if !value.is_finite() || !delivered.knows(id) {
                return;
            }
            let value = unit(value);
            let report = FromAudio::ParamValue {
                id,
                value,
                generation,
            };
            if to_main.push(report).is_err() {
                *lost += 1;
            }
            if output.len() < output.capacity() {
                output.push(Event {
                    offset: place(offset),
                    kind: EventKind::ParamValue {
                        param: ParamId(id),
                        value,
                    },
                });
            } else {
                *lost += 1;
            }
        });
    }
}

/// Whether an event that is waiting for the next block is still to be
/// delivered after a reset: as in the CLAP host, a reset forgets what is
/// about notes and keeps what sets a parameter.
fn outlasts_reset(kind: &EventKind) -> bool {
    matches!(kind, EventKind::ParamValue { .. })
}

impl Processor<f32> for Vst3Processor {
    /// Checks that the plugin can run under `spec`.
    ///
    /// A plugin's sample rate is fixed when it is activated, on the main
    /// thread, so nothing can be changed from here. If `spec` asks for
    /// another rate, every block is silent and reports
    /// [`ProcessFailure::SampleRateMismatch`] until this is called again
    /// with the rate the plugin has. Any block size and channel count are
    /// accepted. Does not call the plugin and does not allocate.
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.wrong_rate = spec.sample_rate != self.sample_rate;
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        self.process_with(buffer, &ProcessContext::empty());
    }

    fn process_with(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.process_with_aux(buffer, context, &mut []);
    }

    /// Asks the plugin to clear its tails and voices, and forgets the notes
    /// kept for the next block.
    ///
    /// VST 3 gives a host no call for this on the audio thread. The plugin
    /// is told that processing stops, and before the next block that it
    /// starts again (`setProcessing`), which VST 3 suggests a plugin use to
    /// clear delay lines and reverbs. Whether it does is up to the plugin:
    /// the framework's own wrapper does not, and is cleared only by
    /// deactivating it. Parameter values waiting for the next block are
    /// still delivered.
    fn reset(&mut self) {
        if self.started {
            // SAFETY: the plugin's active processor, called from the thread
            // that holds this value; VST 3 allows this call there.
            unsafe { self.plugin().setProcessing(0) };
            self.started = false;
        }
        // `retain` moves what stays within the memory the list has.
        self.carry.retain(|event| outlasts_reset(&event.kind));
    }

    /// The latency the plugin reported when it was activated, in frames.
    fn latency_samples(&self) -> usize {
        self.shared.latency() as usize
    }
}

impl Drop for Vst3Processor {
    fn drop(&mut self) {
        if self.started {
            // SAFETY: the plugin's active processor, for the last time from
            // this value; VST 3 allows this call on the audio thread or the
            // main thread.
            unsafe { self.plugin().setProcessing(0) };
        }
        self.shared.processor_dropped(self.from_main.take());
        // If the main handle is gone, what becomes of the plugin is decided
        // here.
        Shared::processor_gone(&self.shared);
    }
}

impl std::fmt::Debug for Vst3Processor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Processor")
            .field("sample_rate", &self.sample_rate)
            .field("max_block_size", &self.max_frames)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

// --- Audio -------------------------------------------------------------------

/// The memory the plugin reads its input from and writes its output to,
/// and the bus tables VST 3 wants it described by.
///
/// The plugin never sees the caller's buffer: input is copied in and output
/// copied out, so the plugin cannot write where it should not, whatever it
/// believes about channel counts.
struct Io {
    inputs: Vec<AudioBuffer<f32>>,
    outputs: Vec<AudioBuffer<f32>>,
    /// A pointer to every channel of every input bus, in order.
    input_pointers: Vec<*mut f32>,
    output_pointers: Vec<*mut f32>,
    /// One entry per input bus, each pointing into `input_pointers`.
    input_buses: Vec<vst::AudioBusBuffers>,
    output_buses: Vec<vst::AudioBusBuffers>,
    main_input: Option<usize>,
    sidechain: Option<usize>,
}

impl Io {
    fn new(
        inputs: &[u32],
        outputs: &[u32],
        main_input: Option<usize>,
        sidechain: Option<usize>,
        max_frames: usize,
    ) -> Self {
        let buffers = |buses: &[u32]| -> Vec<AudioBuffer<f32>> {
            buses
                .iter()
                .map(|&channels| AudioBuffer::new(channels as usize, max_frames))
                .collect()
        };
        let empty = vst::AudioBusBuffers {
            numChannels: 0,
            silenceFlags: 0,
            __field0: vst::AudioBusBuffers__type0 {
                channelBuffers32: ptr::null_mut(),
            },
        };
        let channels = |buses: &[u32]| buses.iter().map(|&channels| channels as usize).sum();
        Self {
            inputs: buffers(inputs),
            outputs: buffers(outputs),
            input_pointers: vec![ptr::null_mut(); channels(inputs)],
            output_pointers: vec![ptr::null_mut(); channels(outputs)],
            input_buses: vec![empty; inputs.len()],
            output_buses: vec![empty; outputs.len()],
            main_input,
            sidechain,
        }
    }

    /// Fills the plugin's inputs with frames `start..start + frames` of the
    /// caller's block and sidechain, clears its outputs, and points the
    /// tables at both. A source that does not reach the end of the piece is
    /// read as silence.
    fn load(
        &mut self,
        buffer: &AudioBuffer<f32>,
        sidechain: Option<&AudioBuffer<f32>>,
        start: usize,
        frames: usize,
    ) {
        let end = start + frames;
        for (index, bus) in self.inputs.iter_mut().enumerate() {
            bus.set_num_frames(frames);
            let source = if Some(index) == self.main_input {
                Some(buffer)
            } else if Some(index) == self.sidechain {
                sidechain
            } else {
                None
            };
            for (channel, samples) in bus.iter_channels_mut().enumerate() {
                let source = source
                    .filter(|source| channel < source.num_channels())
                    .and_then(|source| source.channel(channel).get(start..end));
                match source {
                    Some(source) => samples.copy_from_slice(source),
                    None => samples.fill(0.0),
                }
            }
        }
        for bus in &mut self.outputs {
            bus.set_num_frames(frames);
            bus.clear();
        }
        point(
            &mut self.inputs,
            &mut self.input_pointers,
            &mut self.input_buses,
        );
        point(
            &mut self.outputs,
            &mut self.output_pointers,
            &mut self.output_buses,
        );
    }

    /// Copies the plugin's main output into frames `start..start + frames`
    /// of the caller's block, or silence if `keep` is false.
    fn store(&mut self, buffer: &mut AudioBuffer<f32>, start: usize, frames: usize, keep: bool) {
        let end = start + frames;
        let source = self.outputs.first().filter(|_| keep);
        for (channel, target) in buffer.iter_channels_mut().enumerate() {
            let target = &mut target[start..end];
            match source.filter(|source| channel < source.num_channels()) {
                Some(source) => target.copy_from_slice(source.channel(channel)),
                None => target.fill(0.0),
            }
        }
    }

    /// Copies the plugin's audio output buses after the main one, in their
    /// order, into frames `start..start + frames` of `aux`, one buffer per
    /// bus, or silence if `keep` is false. A bus with no buffer in `aux` is
    /// not copied; channels a buffer has and its bus lacks, and frames past
    /// a buffer's end, are left out the same way. A bus that is switched
    /// off has silent memory here, which is what it gives.
    fn store_aux(&mut self, aux: &mut [AudioBuffer<f32>], start: usize, frames: usize, keep: bool) {
        let end = start + frames;
        for (source, target) in self.outputs.iter().skip(1).zip(aux.iter_mut()) {
            for (channel, target) in target.iter_channels_mut().enumerate() {
                let Some(target) = target.get_mut(start..end) else {
                    continue;
                };
                match (keep && channel < source.num_channels()).then(|| source.channel(channel)) {
                    Some(samples) => target.copy_from_slice(samples),
                    None => target.fill(0.0),
                }
            }
        }
    }
}

/// Points one direction's tables at its buffers.
///
/// The pointers are taken fresh for every piece, from one pass over each
/// buffer, and the buffers are not touched again until the plugin has
/// returned, so no pointer the plugin is given outlives a borrow of the
/// memory it points to.
fn point(
    buffers: &mut [AudioBuffer<f32>],
    pointers: &mut [*mut f32],
    buses: &mut [vst::AudioBusBuffers],
) {
    let mut slot = 0;
    for buffer in buffers.iter_mut() {
        for channel in buffer.iter_channels_mut() {
            if let Some(pointer) = pointers.get_mut(slot) {
                *pointer = channel.as_mut_ptr();
            }
            slot += 1;
        }
    }
    let table = pointers.as_mut_ptr();
    let mut first = 0;
    for (bus, buffer) in buses.iter_mut().zip(buffers.iter()) {
        bus.numChannels = buffer.num_channels() as i32;
        bus.silenceFlags = 0;
        // Within the table, or one past its end for a bus with no channels,
        // which the plugin has no reason to follow.
        bus.__field0.channelBuffers32 = table.wrapping_add(first);
        first += buffer.num_channels();
    }
}

#[cfg(test)]
mod tests {
    use manguebyte_dsp::event::{Expression, NoteRef};

    use super::*;

    #[test]
    fn a_reset_forgets_notes_and_keeps_parameter_values() {
        let note = NoteRef::with_id(0, 60, 1);
        assert!(outlasts_reset(&EventKind::ParamValue {
            param: ParamId(7),
            value: 0.5
        }));
        for forgotten in [
            EventKind::NoteOn {
                note,
                velocity: 1.0,
            },
            EventKind::NoteOff {
                note,
                velocity: 0.0,
            },
            EventKind::NoteExpression {
                note,
                expression: Expression::Tuning,
                value: 1.0,
            },
        ] {
            assert!(!outlasts_reset(&forgotten), "{forgotten:?}");
        }
    }

    #[test]
    fn the_bus_tables_point_at_every_channel_in_order() {
        let mut io = Io::new(&[2, 1], &[2], Some(0), Some(1), 16);
        let mut block = AudioBuffer::<f32>::new(2, 8);
        block.channel_mut(0).fill(1.0);
        block.channel_mut(1).fill(2.0);
        let mut side = AudioBuffer::<f32>::new(1, 8);
        side.channel_mut(0).fill(3.0);
        io.load(&block, Some(&side), 4, 4);
        for (bus, expected) in io.input_buses.iter().zip([[1.0, 2.0].as_slice(), &[3.0]]) {
            assert_eq!(bus.numChannels as usize, expected.len());
            for (channel, &value) in expected.iter().enumerate() {
                // SAFETY: the table holds `numChannels` pointers, each to 4
                // frames of the buffers `io` owns, which nothing else is
                // touching.
                let samples = unsafe {
                    std::slice::from_raw_parts(*bus.__field0.channelBuffers32.add(channel), 4)
                };
                assert_eq!(samples, [value; 4]);
            }
        }
        // The output, all zeros, is copied back over the second half only.
        io.store(&mut block, 4, 4, true);
        assert_eq!(block.channel(0), [1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0]);
        // A failed piece leaves silence.
        block.channel_mut(1).fill(5.0);
        io.store(&mut block, 0, 4, false);
        assert_eq!(&block.channel(1)[..4], [0.0; 4]);
    }
}
