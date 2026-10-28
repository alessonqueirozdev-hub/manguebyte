//! The audio-thread half of a hosted plugin.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::events::*;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::*;
use manguebyte_core::spsc::{Consumer, Producer};
use manguebyte_core::{AudioBuffer, NoDenormals, ParamId};
use manguebyte_dsp::event::{Event, EventKind};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};

use super::events::{
    InEvent, NoteRoute, OutputEvent, OutputKind, ParamSlot, from_clap, held_to_declared, to_clap,
    transport_event,
};
use super::host::{Shared, guard};
use super::plugin::{AudioPortInfo, Tail};
use super::{CARRIED_EVENTS, EVENTS_PER_BLOCK, FromAudio, ProcessFailure, ToAudio};

/// How the last block went.
///
/// CLAP may come to define further statuses, so a `match` on this needs a
/// catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProcessStatus {
    /// No block has been processed yet.
    Idle,
    /// The plugin processed the block and wants to go on being called.
    Continue,
    /// The plugin processed the block, and may be left alone once its
    /// output has gone quiet.
    ContinueIfNotQuiet,
    /// The plugin processed the block and is now only ringing out. See
    /// [`ClapProcessor::tail`] for how long that takes.
    Tail,
    /// The plugin processed the block and needs no further calls until
    /// something reaches it: an event, or audio at its input.
    Sleep,
    /// The block is silent because the plugin failed.
    Failed(ProcessFailure),
}

/// The functions and tables the audio thread calls.
pub(crate) struct Calls {
    pub(crate) plugin: *const clap_plugin,
    pub(crate) table: clap_plugin,
    pub(crate) process:
        unsafe extern "C" fn(*const clap_plugin, *const clap_process) -> clap_process_status,
    pub(crate) flush: Option<
        unsafe extern "C" fn(
            *const clap_plugin,
            *const clap_input_events,
            *const clap_output_events,
        ),
    >,
    pub(crate) tail: Option<unsafe extern "C" fn(*const clap_plugin) -> u32>,
}

/// Everything [`ClapProcessor::new`] needs.
pub(crate) struct Setup<'a> {
    pub(crate) shared: Arc<Shared>,
    pub(crate) calls: Calls,
    pub(crate) sample_rate: f64,
    pub(crate) max_frames: usize,
    pub(crate) inputs: &'a [AudioPortInfo],
    pub(crate) outputs: &'a [AudioPortInfo],
    pub(crate) params: Vec<ParamSlot>,
    pub(crate) route: NoteRoute,
    pub(crate) from_main: Consumer<ToAudio>,
    pub(crate) to_main: Producer<FromAudio>,
}

/// A hosted plugin as the audio thread deals with it.
///
/// It is a [`Processor`]: put it in a device callback, a chain or a
/// `manguebyte-graph` graph like any other. It is made by
/// [`ClapPlugin::activate`](super::ClapPlugin::activate), and is the only
/// way to process audio with the plugin, so the compiler keeps processing
/// on one thread at a time and everything else on the main thread.
///
/// # Real-time safety
///
/// [`process`](Processor::process), [`process_with`](Processor::process_with),
/// [`reset`](Processor::reset) and [`flush`](Self::flush) do not allocate,
/// lock or block in this crate's code: buffers, pointer tables and event
/// lists are all allocated at activation. What the plugin does inside its
/// own functions is up to the plugin. Subnormal numbers are flushed to zero
/// while the plugin runs, as in the framework's own wrappers.
///
/// Dropping the processor is *not* real-time safe: it frees its buffers
/// and tells the plugin to stop processing. Drop it away from the audio
/// thread; a graph does that by itself.
///
/// # How a block is laid out
///
/// The buffer passed to `process_with` is the plugin's main input on the
/// way in and its main output on the way out. Channels the plugin does not
/// have are silent, and channels the buffer does not have are discarded.
/// [`ProcessContext::sidechain`] feeds the plugin's first input port that
/// is not its main one. A sidechain with fewer frames than the block cannot
/// be lined up with it, and is treated as not connected for the whole of
/// that block. A block longer than the size the plugin was activated with
/// is processed in pieces of that size, with every event still on its
/// frame; the output does not depend on where the pieces fall.
///
/// An event placed at or beyond the end of the block is kept and delivered
/// on the first frame of the next block, so it is not lost.
///
/// # Events
///
/// Every event in [`ProcessContext::events`] is converted to its CLAP
/// counterpart and handed to the plugin on the frame it carries.
///
/// - A parameter value or a modulation amount is in the *plugin's* scale:
///   the range between [`ParamInfo::min`](super::ParamInfo::min) and
///   [`ParamInfo::max`](super::ParamInfo::max) that the plugin declares for
///   that parameter. A value is held to that range. The plugin is not asked
///   what its flags allow; that is for the application to respect.
/// - A note keeps its channel, key and id. Notes and note expression go to
///   the plugin's first note input, as CLAP note events if it takes them
///   and as MIDI if it takes only that.
/// - Pan is converted between the framework's `-1..=1` and CLAP's `0..=1`.
///   Every note expression is held to the range CLAP gives it: a volume of
///   0 to 4, a tuning of 120 semitones either way, 0 to 1 for the others.
///
/// An event the plugin has no way to receive is not sent: a note for a
/// plugin with no note input, a value for a parameter it does not have, a
/// number that is not one. These are counted in
/// [`ClapPlugin::undeliverable_events`](super::ClapPlugin::undeliverable_events).
/// The plugin is handed at most
/// [`EVENTS_PER_BLOCK`](super::EVENTS_PER_BLOCK) events in one call, which
/// is one piece of a block. Further ones are dropped, and counted in
/// [`ClapPlugin::lost_events`](super::ClapPlugin::lost_events).
///
/// What the plugin reports in return is in
/// [`output_events`](Self::output_events) after each call.
///
/// # When the plugin fails
///
/// The block is silent, [`status`](Self::status) says why, and the main
/// thread sees the same from [`ClapPlugin::pump`](super::ClapPlugin::pump).
/// The processor goes on calling the plugin on later blocks.
///
/// A plugin that reports an error was called, and has had the events of
/// that block. A plugin that could not be called at all, because it refused
/// to start or because the processor was prepared for another sample rate,
/// has not: the events of such a block are kept, and delivered on the first
/// frame of the next block the plugin does process. So a note-off or a
/// parameter value that falls in a silent block arrives late, and is not
/// lost. 1,024 events can wait in this way; beyond that they are dropped,
/// and counted in
/// [`ClapPlugin::lost_events`](super::ClapPlugin::lost_events).
///
///
/// # Example
///
/// The example in the [module documentation](super) hosts a plugin from
/// loading to deactivation, and runs as a test. What the compiler knows
/// about the audio half is this:
///
/// ```
/// use manguebyte_dsp::Processor;
/// use manguebyte_host::clap::ClapProcessor;
///
/// // A processor like any other, which can be sent to the thread that
/// // runs the audio.
/// fn is_a_processor<T: Processor<f32> + Send>() {}
/// is_a_processor::<ClapProcessor>();
/// ```
pub struct ClapProcessor {
    shared: Arc<Shared>,
    calls: Calls,
    sample_rate: f64,
    max_frames: usize,
    io: Io,
    /// The events of the piece being processed, as the plugin reads them.
    input: Vec<InEvent>,
    /// Where the plugin's own events go. It also holds the plugin's
    /// parameters, which both directions are checked against.
    sink: Sink,
    /// Events still to be delivered: the ones that fell beyond the end of
    /// the last block, and the ones of blocks the plugin was not called
    /// for.
    carry: Vec<Event>,
    route: NoteRoute,
    /// The main thread's queue. `None` only while being dropped.
    from_main: Option<Consumer<ToAudio>>,
    transport: clap_event_transport,
    /// Frames processed since activation.
    steady_time: i64,
    /// Events not sent since the count was last passed on, because the
    /// plugin has no way to receive them.
    undeliverable: u64,
    started: bool,
    wrong_rate: bool,
    status: ProcessStatus,
}

// SAFETY: the raw pointers in here are the plugin, its functions, and
// tables that point into memory this value owns. CLAP lets any thread call
// a plugin's audio-thread functions as long as only one does at a time,
// and every such call here goes through `&mut self`.
unsafe impl Send for ClapProcessor {}

impl ClapProcessor {
    /// Allocates everything processing will need.
    pub(crate) fn new(setup: Setup<'_>) -> Self {
        setup.shared.processor_created();
        Self {
            io: Io::new(setup.inputs, setup.outputs, setup.max_frames),
            input: Vec::with_capacity(EVENTS_PER_BLOCK),
            sink: Sink {
                events: Vec::with_capacity(EVENTS_PER_BLOCK),
                framework: Vec::with_capacity(EVENTS_PER_BLOCK),
                to_main: setup.to_main,
                params: setup.params,
                base: 0,
                frames: 0,
                lost: 0,
                unlisted: 0,
            },
            carry: Vec::with_capacity(CARRIED_EVENTS),
            route: setup.route,
            from_main: Some(setup.from_main),
            transport: transport_event(&Transport::STOPPED, setup.sample_rate),
            steady_time: 0,
            undeliverable: 0,
            started: false,
            wrong_rate: false,
            status: ProcessStatus::Idle,
            shared: setup.shared,
            calls: setup.calls,
            sample_rate: setup.sample_rate,
            max_frames: setup.max_frames,
        }
    }

    /// What the plugin reported during the last call to
    /// [`process_with`](Processor::process_with) or [`flush`](Self::flush),
    /// in the order it reported it.
    ///
    /// At most [`EVENTS_PER_BLOCK`](super::EVENTS_PER_BLOCK) are kept for
    /// one call, however many pieces the block was processed in; the plugin
    /// is told when one does not fit.
    ///
    /// A parameter value is held to the range the plugin declared for that
    /// parameter. A value or a gesture for a parameter the plugin does not
    /// list is not kept, and
    /// [`ClapPlugin::pump`](super::ClapPlugin::pump) remarks on it.
    /// Real-time safe.
    pub fn output_events(&self) -> &[OutputEvent] {
        &self.sink.events
    }

    /// The [`OutputKind::Event`]s of [`output_events`](Self::output_events),
    /// as events, in the same order: kept apart while processing, so that
    /// reading them allocates nothing and converts nothing.
    pub(crate) fn framework_events(&self) -> &[Event] {
        &self.sink.framework
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

    /// How long the plugin goes on sounding after its input stops, or
    /// `None` if it does not say. Real-time safe as far as this crate's
    /// code goes.
    pub fn tail(&self) -> Option<Tail> {
        let get = self.calls.tail?;
        let previous = self.shared.take_audio_role();
        // SAFETY: the plugin is alive for as long as this value is, and
        // CLAP allows this function on the audio thread, whose role this
        // thread holds.
        let frames = unsafe { get(self.calls.plugin) };
        self.shared.give_audio_role(previous);
        Some(Tail::from_frames(frames))
    }

    /// Delivers the parameter changes waiting from the main thread without
    /// processing any audio, and collects what the plugin reports in
    /// return into [`output_events`](Self::output_events).
    ///
    /// For a plugin that is active but not being processed, for example
    /// because it went to sleep. Does nothing if the plugin has no
    /// parameters. Real-time safe as far as this crate's code goes.
    pub fn flush(&mut self) {
        let Some(flush) = self.calls.flush else {
            return;
        };
        let previous = self.shared.take_audio_role();
        self.sink.clear();
        self.sink.base = 0;
        self.sink.frames = 1;
        self.input.clear();
        self.take_from_main();

        let in_events = clap_input_events {
            ctx: (&raw mut self.input).cast(),
            size: Some(input_size),
            get: Some(input_get),
        };
        let out_events = clap_output_events {
            ctx: (&raw mut self.sink).cast(),
            try_push: Some(sink_push),
        };
        // SAFETY: the plugin is alive and active for as long as this value
        // exists, and this thread holds the audio role. Both lists point
        // at fields of `self` that nothing else touches until the call
        // returns.
        unsafe {
            flush(
                self.calls.plugin,
                &raw const in_events,
                &raw const out_events,
            )
        };
        self.note_lost();
        self.shared.give_audio_role(previous);
    }

    /// Queues one event for the plugin, at `time` frames into the piece.
    fn queue(&mut self, kind: &EventKind, time: u32) {
        let Some(event) = to_clap(kind, time, &self.sink.params, self.route) else {
            self.undeliverable += 1;
            return;
        };
        if self.input.len() < self.input.capacity() {
            self.input.push(event);
        } else {
            self.sink.lost += 1;
        }
    }

    /// Moves what the main thread sent into the input list, on frame 0.
    fn take_from_main(&mut self) {
        while let Some(message) = self.from_main.as_mut().and_then(Consumer::pop) {
            let ToAudio::ParamValue { id, value } = message;
            let kind = EventKind::ParamValue {
                param: ParamId(id),
                value,
            };
            self.queue(&kind, 0);
        }
    }

    /// Moves the events kept from the last block into the input list, on
    /// frame 0.
    fn take_carried(&mut self) {
        let mut index = 0;
        while let Some(event) = self.carry.get(index).copied() {
            self.queue(&event.kind, 0);
            index += 1;
        }
        self.carry.clear();
    }

    /// Keeps events for the first frame of the next block the plugin
    /// processes, in the order given, behind the ones already waiting. An
    /// event there is no room for is counted as lost.
    fn keep(&mut self, events: &[Event]) {
        for event in events {
            // The list was allocated with room for this many, so a push
            // below the limit does not allocate.
            if self.carry.len() < CARRIED_EVENTS {
                self.carry.push(*event);
            } else {
                self.sink.lost += 1;
            }
        }
    }

    /// Adds the events that were dropped, the ones that could not be sent
    /// and the reports that were refused to the counts the main thread
    /// reads.
    fn note_lost(&mut self) {
        if self.sink.lost > 0 {
            self.shared
                .lost_events
                .fetch_add(self.sink.lost, Ordering::Relaxed);
            self.sink.lost = 0;
        }
        if self.undeliverable > 0 {
            self.shared
                .undeliverable_events
                .fetch_add(self.undeliverable, Ordering::Relaxed);
            self.undeliverable = 0;
        }
        if self.sink.unlisted > 0 {
            self.shared
                .unlisted_reports
                .fetch_add(self.sink.unlisted, Ordering::Relaxed);
            self.sink.unlisted = 0;
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
        if self.started {
            return true;
        }
        self.started = match self.calls.table.start_processing {
            // SAFETY: the plugin is alive and active, it is not processing,
            // and this thread holds the audio role.
            Some(start) => unsafe { start(self.calls.plugin) },
            None => true,
        };
        self.started
    }

    /// Ends a block the plugin cannot be called for: silence at the output,
    /// the reason on record, and the block's events kept for the next block
    /// the plugin does process, so that they arrive late and not never.
    fn skip(&mut self, buffer: &mut AudioBuffer<f32>, events: &[Event], failure: ProcessFailure) {
        buffer.clear();
        self.keep(events);
        self.fail(failure);
        self.note_lost();
    }

    fn run(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.sink.clear();
        let events = context.events.as_slice();
        if self.wrong_rate {
            self.skip(buffer, events, ProcessFailure::SampleRateMismatch);
            return;
        }
        if !self.start() {
            self.skip(buffer, events, ProcessFailure::StartRefused);
            return;
        }

        let total = buffer.num_frames();
        // A sidechain shorter than the block cannot be lined up with it,
        // and is treated as not connected. That is settled here, once for
        // the block, so that the answer is the same for every piece and
        // does not depend on the size the plugin was activated with.
        let sidechain = context
            .sidechain
            .filter(|sidechain| sidechain.num_frames() >= total);
        // The first event not yet handed to the plugin.
        let mut next = 0;
        let mut transport = *context.transport;
        let mut failed = false;
        let mut start = 0;
        while start < total {
            let frames = (total - start).min(self.max_frames);
            let end = start + frames;

            self.input.clear();
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

            self.io.load(buffer, sidechain, start, frames);
            self.transport = transport_event(&transport, self.sample_rate);
            let good = self.call(frames as u32, start as u32);
            self.io.store(buffer, start, frames, good);
            failed |= !good;

            self.steady_time = self.steady_time.saturating_add(frames as i64);
            start = end;
            if start < total {
                // Only a further piece needs to know where the music has
                // got to. The caller's own transport is never moved.
                advance(&mut transport, frames, self.sample_rate);
            }
        }

        // Whatever belongs after the last frame waits for the next block.
        self.keep(events.get(next..).unwrap_or_default());
        if failed {
            self.fail(ProcessFailure::Error);
        }
        self.note_lost();
    }

    /// Calls the plugin for one piece of `frames` frames that starts
    /// `base` frames into the caller's block. Returns whether the plugin's
    /// output is to be kept.
    fn call(&mut self, frames: u32, base: u32) -> bool {
        self.sink.base = base;
        self.sink.frames = frames;
        let in_events = clap_input_events {
            ctx: (&raw mut self.input).cast(),
            size: Some(input_size),
            get: Some(input_get),
        };
        let out_events = clap_output_events {
            ctx: (&raw mut self.sink).cast(),
            try_push: Some(sink_push),
        };
        let process = clap_process {
            steady_time: self.steady_time,
            frames_count: frames,
            transport: &raw const self.transport,
            audio_inputs: self.io.input_ports.as_ptr(),
            audio_outputs: self.io.output_ports.as_mut_ptr(),
            audio_inputs_count: self.io.input_ports.len() as u32,
            audio_outputs_count: self.io.output_ports.len() as u32,
            in_events: &raw const in_events,
            out_events: &raw const out_events,
        };
        // SAFETY: the plugin is alive, active and has been told processing
        // started, and this thread holds the audio role. Every pointer in
        // `process` is to memory this value owns or to a local above; the
        // port tables were pointed at `frames` frames of scratch memory by
        // `Io::load`, and nothing touches any of it until the call returns.
        let status = unsafe { (self.calls.process)(self.calls.plugin, &raw const process) };
        self.status = match status {
            CLAP_PROCESS_CONTINUE => ProcessStatus::Continue,
            CLAP_PROCESS_CONTINUE_IF_NOT_QUIET => ProcessStatus::ContinueIfNotQuiet,
            CLAP_PROCESS_TAIL => ProcessStatus::Tail,
            CLAP_PROCESS_SLEEP => ProcessStatus::Sleep,
            // An error, or a number that is not a status at all. Either
            // way CLAP says the output is to be thrown away.
            _ => return false,
        };
        true
    }
}

/// Moves `transport` on by `frames`, as [`Transport::advance`] does, for
/// the next piece of a block that is processed in pieces. Real-time safe.
///
/// `Transport::advance` adds to the position in frames with a plain `+`,
/// which overflows for a position within a block of the end of its range,
/// and in a build with overflow checks that is a panic on the audio thread.
/// So the position in frames is taken out, moved on here with an addition
/// that stops at the end of the range, and put back. What is left for
/// `Transport::advance` is arithmetic on floats, which cannot panic, and is
/// done exactly as a caller that cut the block itself would have had it
/// done.
fn advance(transport: &mut Transport, frames: usize, sample_rate: f64) {
    let position = transport.position_samples.take();
    transport.advance(frames, sample_rate);
    transport.position_samples = match position {
        // A stopped transport stays where it is, in frames as in beats.
        Some(position) if transport.playing => {
            Some(position.saturating_add(i64::try_from(frames).unwrap_or(i64::MAX)))
        }
        position => position,
    };
}

/// Whether an event that is waiting for the next block is still to be
/// delivered after a reset.
///
/// A reset clears voices and tails and leaves parameters alone. Whatever
/// is about a note goes with the voices. Whatever sets a parameter for the
/// plugin as a whole stays: the application has said what the parameter is
/// to be, and the plugin has yet to hear it.
fn outlasts_reset(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::ParamValue { .. } | EventKind::ParamModulation { note: None, .. }
    )
}

impl Processor<f32> for ClapProcessor {
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
        let previous = self.shared.take_audio_role();
        {
            // Numbers too small to hear are zero while the plugin runs,
            // whether or not the plugin thought of it.
            let _quiet = NoDenormals::new();
            self.run(buffer, context);
        }
        self.shared.give_audio_role(previous);
    }

    /// Tells the plugin to clear its tails and voices, and forgets the
    /// notes kept for the next block.
    ///
    /// A reset drops what is sounding and leaves parameters as they are,
    /// and the events waiting for the next block are treated alike. Notes,
    /// note expression, MIDI and modulation for one note are forgotten, on
    /// purpose and without being counted as lost. A parameter value, and a
    /// modulation that is not for one note, are still delivered on the
    /// first frame of the next block.
    fn reset(&mut self) {
        let previous = self.shared.take_audio_role();
        if let Some(reset) = self.calls.table.reset {
            // SAFETY: the plugin is alive and active, and this thread holds
            // the audio role.
            unsafe { reset(self.calls.plugin) };
        }
        // `retain` moves what stays within the memory the list has.
        self.carry.retain(|event| outlasts_reset(&event.kind));
        self.shared.give_audio_role(previous);
    }

    /// The latency the plugin reported when it was activated, in frames.
    fn latency_samples(&self) -> usize {
        self.shared.latency() as usize
    }
}

impl Drop for ClapProcessor {
    fn drop(&mut self) {
        if self.started {
            let previous = self.shared.take_audio_role();
            if let Some(stop) = self.calls.table.stop_processing {
                // SAFETY: the plugin is alive, active and processing, and
                // this thread holds the audio role, for the last time.
                unsafe { stop(self.calls.plugin) };
            }
            self.shared.give_audio_role(previous);
        }
        self.shared.processor_dropped(self.from_main.take());
        // If the main handle is gone, what becomes of the plugin is decided
        // here; see the function for how.
        Shared::processor_gone(&self.shared);
    }
}

impl std::fmt::Debug for ClapProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapProcessor")
            .field("sample_rate", &self.sample_rate)
            .field("max_block_size", &self.max_frames)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

// --- Audio ---------------------------------------------------------------

/// The memory the plugin reads its input from and writes its output to,
/// and the tables of pointers CLAP wants it described by.
///
/// The plugin never sees the caller's buffer. Input is copied in and output
/// copied out, which costs one copy per channel and means the plugin cannot
/// write where it should not, whatever it believes about channel counts.
struct Io {
    /// One buffer per input port of the plugin.
    inputs: Vec<AudioBuffer<f32>>,
    /// One buffer per output port of the plugin.
    outputs: Vec<AudioBuffer<f32>>,
    /// A pointer to every channel of every input port, in order.
    input_pointers: Vec<*mut f32>,
    output_pointers: Vec<*mut f32>,
    /// One entry per input port, each pointing into `input_pointers`.
    input_ports: Vec<clap_audio_buffer>,
    output_ports: Vec<clap_audio_buffer>,
    /// Which input port is the main one, if the plugin has one.
    main_input: Option<usize>,
    /// The first input port that is not the main one.
    sidechain: Option<usize>,
    /// Which output port is heard.
    main_output: Option<usize>,
}

impl Io {
    fn new(inputs: &[AudioPortInfo], outputs: &[AudioPortInfo], max_frames: usize) -> Self {
        let buffers = |ports: &[AudioPortInfo]| -> Vec<AudioBuffer<f32>> {
            ports
                .iter()
                .map(|port| AudioBuffer::new(port.channels as usize, max_frames))
                .collect()
        };
        let empty_port = clap_audio_buffer {
            data32: ptr::null_mut(),
            data64: ptr::null_mut(),
            channel_count: 0,
            latency: 0,
            constant_mask: 0,
        };
        let channels =
            |ports: &[AudioPortInfo]| ports.iter().map(|port| port.channels as usize).sum();
        let main_input = inputs.first().filter(|port| port.is_main).map(|_| 0);
        Self {
            inputs: buffers(inputs),
            outputs: buffers(outputs),
            input_pointers: vec![ptr::null_mut(); channels(inputs)],
            output_pointers: vec![ptr::null_mut(); channels(outputs)],
            input_ports: vec![empty_port; inputs.len()],
            output_ports: vec![empty_port; outputs.len()],
            main_input,
            sidechain: (0..inputs.len()).find(|&index| Some(index) != main_input),
            // The first output is the one that is heard, flagged or not.
            main_output: (!outputs.is_empty()).then_some(0),
        }
    }

    /// Fills the plugin's inputs with frames `start..start + frames` of the
    /// caller's block and sidechain, clears its outputs, and points the
    /// tables at both.
    ///
    /// The caller passes a sidechain only if it is at least as long as the
    /// whole block. A source that does not reach the end of the piece all
    /// the same is read as silence, so nothing here indexes past the end of
    /// a buffer.
    fn load(
        &mut self,
        buffer: &AudioBuffer<f32>,
        sidechain: Option<&AudioBuffer<f32>>,
        start: usize,
        frames: usize,
    ) {
        let end = start + frames;
        for (index, port) in self.inputs.iter_mut().enumerate() {
            port.set_num_frames(frames);
            let source = if Some(index) == self.main_input {
                Some(buffer)
            } else if Some(index) == self.sidechain {
                sidechain
            } else {
                None
            };
            for (channel, samples) in port.iter_channels_mut().enumerate() {
                let source = source
                    .filter(|source| channel < source.num_channels())
                    .and_then(|source| source.channel(channel).get(start..end));
                match source {
                    Some(source) => samples.copy_from_slice(source),
                    None => samples.fill(0.0),
                }
            }
        }
        for port in &mut self.outputs {
            port.set_num_frames(frames);
            port.clear();
        }
        point(
            &mut self.inputs,
            &mut self.input_pointers,
            &mut self.input_ports,
        );
        point(
            &mut self.outputs,
            &mut self.output_pointers,
            &mut self.output_ports,
        );
    }

    /// Copies the plugin's main output into frames `start..start + frames`
    /// of the caller's block, or silence if `keep` is false.
    fn store(&mut self, buffer: &mut AudioBuffer<f32>, start: usize, frames: usize, keep: bool) {
        let end = start + frames;
        let source = self.main_output.filter(|_| keep).map(|index| {
            let constant = self.output_ports[index].constant_mask;
            (&mut self.outputs[index], constant)
        });
        let Some((source, constant)) = source else {
            for channel in buffer.iter_channels_mut() {
                channel[start..end].fill(0.0);
            }
            return;
        };
        // A plugin may say a channel holds one value throughout, in which
        // case only its first sample is defined.
        for (channel, samples) in source.iter_channels_mut().enumerate().take(64) {
            if constant & (1 << channel) != 0 {
                if let Some(&value) = samples.first() {
                    samples.fill(value);
                }
            }
        }
        for (channel, target) in buffer.iter_channels_mut().enumerate() {
            let target = &mut target[start..end];
            if channel < source.num_channels() {
                target.copy_from_slice(source.channel(channel));
            } else {
                target.fill(0.0);
            }
        }
    }
}

/// Points one direction's tables at its buffers.
///
/// The pointers are taken fresh for every piece, from one pass over each
/// buffer, and the buffers are not touched again until the plugin has
/// returned. So no pointer the plugin is given ever outlives a borrow of
/// the memory it points to.
fn point(
    buffers: &mut [AudioBuffer<f32>],
    pointers: &mut [*mut f32],
    ports: &mut [clap_audio_buffer],
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
    for (port, buffer) in ports.iter_mut().zip(buffers.iter()) {
        // Within the table, or one past its end for a port with no
        // channels, which the plugin has no reason to follow.
        port.data32 = table.wrapping_add(first);
        port.data64 = ptr::null_mut();
        port.channel_count = buffer.num_channels() as u32;
        port.latency = 0;
        port.constant_mask = 0;
        first += buffer.num_channels();
    }
}

// --- Events --------------------------------------------------------------

/// Where the events a plugin pushes while processing end up.
struct Sink {
    /// The events of the current call, for the caller on the audio thread.
    events: Vec<OutputEvent>,
    /// Those of them the framework's own event type can say, as such, for
    /// [`AnyProcessor`](crate::any::AnyProcessor). Never longer than
    /// `events`, so never short of room when it has some.
    framework: Vec<Event>,
    /// Parameter changes and gestures also go to the main thread.
    to_main: Producer<FromAudio>,
    /// The plugin's parameters, sorted by id. What the plugin reports about
    /// a parameter is held against this list; the events on their way to
    /// the plugin are converted with it too.
    params: Vec<ParamSlot>,
    /// Where the piece being processed starts within the caller's block.
    base: u32,
    /// How long that piece is.
    frames: u32,
    /// Events dropped because a list or a queue was full.
    lost: u64,
    /// Reports refused because they were about a parameter the plugin does
    /// not list.
    unlisted: u64,
}

impl Sink {
    /// Takes one event from the plugin. Returns what the plugin is to be
    /// told: whether the event was taken.
    fn accept(&mut self, time: u32, kind: OutputKind) -> bool {
        let Some(kind) = held_to_declared(kind, &self.params) else {
            // About a parameter that is not in the plugin's list, so there
            // is nothing the application could do with it. It is counted
            // for the main thread to remark on. The plugin could do nothing
            // about a refusal, so it is told the event was taken.
            self.unlisted += 1;
            return true;
        };
        let report = match kind {
            OutputKind::Event(EventKind::ParamValue { param, value }) => {
                Some(FromAudio::ParamValue { id: param.0, value })
            }
            OutputKind::GestureBegin { param } => Some(FromAudio::GestureBegin(param.0)),
            OutputKind::GestureEnd { param } => Some(FromAudio::GestureEnd(param.0)),
            _ => None,
        };
        if let Some(report) = report {
            if self.to_main.push(report).is_err() {
                self.lost += 1;
            }
        }
        if self.events.len() >= self.events.capacity() {
            self.lost += 1;
            return false;
        }
        // A time beyond the piece is the plugin's mistake; the event is
        // kept, on the last frame it could have meant.
        let offset = self
            .base
            .saturating_add(time.min(self.frames.saturating_sub(1)));
        self.events.push(OutputEvent { offset, kind });
        if let OutputKind::Event(kind) = kind {
            self.framework.push(Event { offset, kind });
        }
        true
    }

    /// Forgets the last call's events.
    fn clear(&mut self) {
        self.events.clear();
        self.framework.clear();
    }
}

/// The input list behind a pointer the plugin passed back.
///
/// # Safety
///
/// `list` must be null or the list this module handed the plugin for the
/// call that is in progress.
unsafe fn input_list<'a>(list: *const clap_input_events) -> Option<&'a Vec<InEvent>> {
    if list.is_null() {
        return None;
    }
    // SAFETY: per the contract, `ctx` is the address of the `Vec` the list
    // was made from, which is not touched by anything else during the call.
    unsafe { (*list).ctx.cast::<Vec<InEvent>>().as_ref() }
}

pub(crate) unsafe extern "C" fn input_size(list: *const clap_input_events) -> u32 {
    guard(0, || {
        // SAFETY: the plugin passes the list it was given.
        unsafe { input_list(list) }.map_or(0, |events| events.len() as u32)
    })
}

pub(crate) unsafe extern "C" fn input_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    guard(ptr::null(), || {
        // SAFETY: the plugin passes the list it was given.
        unsafe { input_list(list) }
            .and_then(|events| events.get(index as usize))
            .map_or(ptr::null(), InEvent::header)
    })
}

unsafe extern "C" fn sink_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    guard(false, || {
        if list.is_null() {
            return false;
        }
        // SAFETY: the plugin passes the list it was given, whose `ctx` is
        // the address of the `Sink` it was made from. Nothing else touches
        // that `Sink` during the call, and CLAP has the plugin push from
        // the audio thread only, so this is the one reference to it.
        let Some(sink) = (unsafe { (*list).ctx.cast::<Sink>().as_mut() }) else {
            return false;
        };
        // SAFETY: the plugin passes an event that is valid for this call
        // and as large as its header says, or null.
        match unsafe { from_clap(event) } {
            Some((time, kind)) => sink.accept(time, kind),
            // Nothing this host has a use for. The plugin could do nothing
            // about a refusal, so it is told the event was taken.
            None => true,
        }
    })
}

/// Lets the main thread reuse the input list callbacks for a flush of its
/// own.
pub(crate) fn input_events(events: &mut Vec<InEvent>) -> clap_input_events {
    clap_input_events {
        ctx: ptr::from_mut(events).cast::<c_void>(),
        size: Some(input_size),
        get: Some(input_get),
    }
}

#[cfg(test)]
mod tests {
    use manguebyte_dsp::event::{Expression, NoteRef};
    use manguebyte_midi::MidiMessage;

    use super::*;

    #[test]
    fn a_piece_moves_the_transport_exactly_as_the_framework_does() {
        let mut ours = Transport::playing_at(97.0);
        ours.time_signature = Some((6, 8));
        ours.position_beats = Some(7.25);
        ours.position_samples = Some(-500);
        ours.loop_beats = Some((4.0, 8.0));
        let mut theirs = ours;
        for frames in [1, 64, 7, 4_096, 100_000, 0, 1 << 20] {
            advance(&mut ours, frames, 44_100.0);
            theirs.advance(frames, 44_100.0);
            assert_eq!(ours, theirs, "after a piece of {frames} frames");
            assert_eq!(
                ours.position_beats.map(f64::to_bits),
                theirs.position_beats.map(f64::to_bits)
            );
        }
        assert_eq!(ours.position_samples, Some(-500 + 1_152_744));

        // A stopped transport stays where it is.
        let mut stopped = ours;
        stopped.playing = false;
        advance(&mut stopped, 64, 44_100.0);
        assert_eq!(stopped.position_samples, ours.position_samples);
        assert_eq!(stopped.position_beats, ours.position_beats);

        // A position that is not known does not become known.
        let mut unknown = Transport::STOPPED;
        unknown.playing = true;
        advance(&mut unknown, 64, 48_000.0);
        assert_eq!(unknown, {
            let mut expected = Transport::STOPPED;
            expected.playing = true;
            expected
        });
    }

    #[test]
    fn a_position_at_the_end_of_its_range_stops_there() {
        for (position, frames, expected) in [
            (i64::MAX - 10, 64, i64::MAX),
            (i64::MAX, 1, i64::MAX),
            (i64::MAX - 64, 64, i64::MAX),
            (i64::MAX - 65, 64, i64::MAX - 1),
            (i64::MAX, usize::MAX, i64::MAX),
            (0, usize::MAX, i64::MAX),
            (i64::MIN, 64, i64::MIN + 64),
        ] {
            let mut transport = Transport::playing_at(120.0);
            transport.position_samples = Some(position);
            advance(&mut transport, frames, 48_000.0);
            assert_eq!(
                transport.position_samples,
                Some(expected),
                "{position} moved on by {frames}"
            );
            // The position in beats went on as usual.
            assert!(transport.position_beats > Some(0.0));
        }
    }

    #[test]
    fn a_reset_forgets_notes_and_keeps_what_sets_a_parameter() {
        let note = NoteRef::with_id(0, 60, 1);
        let param = ParamId(7);
        for kept in [
            EventKind::ParamValue { param, value: 0.5 },
            EventKind::ParamModulation {
                param,
                amount: 0.1,
                note: None,
            },
        ] {
            assert!(outlasts_reset(&kept), "{kept:?}");
        }
        for forgotten in [
            EventKind::NoteOn {
                note,
                velocity: 1.0,
            },
            EventKind::NoteOff {
                note,
                velocity: 0.0,
            },
            EventKind::NoteChoke { note },
            EventKind::NoteExpression {
                note,
                expression: Expression::Tuning,
                value: 1.0,
            },
            EventKind::ParamModulation {
                param,
                amount: 0.1,
                note: Some(note),
            },
            EventKind::Midi(MidiMessage::ControlChange {
                channel: 0,
                controller: 1,
                value: 2,
            }),
        ] {
            assert!(!outlasts_reset(&forgotten), "{forgotten:?}");
        }
    }
}
