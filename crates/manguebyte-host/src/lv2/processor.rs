//! The audio half of a hosted LV2 plugin.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use manguebyte_core::spsc::Consumer;
use manguebyte_core::{AudioBuffer, NoDenormals};
use manguebyte_dsp::event::{Event, EventKind};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};

use super::atom::{AtomBuffer, Urids};
use super::plugin::{Layout, Lv2Param, ParamSlot, Shared, ToAudio, clamp_latency, find_param};
use super::{CARRIED_EVENTS, EVENTS_PER_BLOCK};
use crate::delivery::{Sent, advance, midi_of};

/// Everything [`Lv2Processor::new`] needs.
pub(crate) struct Setup<'a> {
    pub(crate) shared: Arc<Shared>,
    pub(crate) layout: Arc<Layout>,
    pub(crate) slots: Arc<[ParamSlot]>,
    pub(crate) params: &'a [Lv2Param],
    pub(crate) values: &'a [f32],
    pub(crate) sample_rate: f64,
    pub(crate) max_frames: usize,
    pub(crate) from_main: Consumer<ToAudio>,
}

/// A hosted LV2 plugin as the audio thread deals with it: a
/// [`Processor`], made by [`Lv2Plugin::activate`](super::Lv2Plugin::activate).
///
/// # Real-time safety
///
/// [`process_with`](Processor::process_with) does not allocate, lock or
/// block in this crate's code: every buffer, the atom sequences and the
/// list of carried events are allocated at activation, and parameter
/// changes from the other half arrive through a wait-free queue. What the
/// plugin does in its own `run` is up to the plugin. Subnormal numbers are
/// flushed to zero while it runs.
///
/// [`reset`](Processor::reset) deactivates and activates the instance,
/// which is how LV2 clears a plugin: what the plugin does in those two
/// functions is up to it (the framework's own wrapper prepares its plugin
/// again, which allocates). It takes no lock that can make it wait: if the
/// other half is saving the plugin's state at that moment, the reset is
/// skipped.
///
/// Dropping the processor is not real-time safe: it deactivates the
/// instance and frees the buffers.
///
/// # How a block is laid out
///
/// The buffer is the plugin's main input on the way in and its main output
/// on the way out, one channel per LV2 audio port, in port order; audio
/// inputs marked `lv2:isSideChain` read [`ProcessContext::sidechain`].
/// Channels the plugin does not have are silent, and channels the buffer
/// does not have are discarded. The plugin always gets inputs and outputs
/// apart, as a plugin marked `lv2:inPlaceBroken` needs. CV inputs are
/// silent and CV outputs are discarded.
///
/// # Events, on their frames
///
/// LV2 control ports hold one value for a whole call of `run`, so a block
/// is run in pieces: one starts at every frame where a parameter changes,
/// and none is longer than the size the plugin was activated with. Each
/// piece's MIDI goes into the plugin's event input as an atom sequence,
/// each message on its frame, after a `time:Position` with the transport
/// at the start of the piece if the input takes one. So every event lands
/// on its frame, and a plugin whose sound does not depend on how a host
/// cuts the timeline sounds the same, to the bit, as with no host at all.
///
/// On one frame, parameter changes come before MIDI, and of two changes to
/// one parameter the later is what the plugin reads.
///
/// - [`EventKind::ParamValue`] is in the port's unit and held to its range.
/// - Notes become MIDI 1.0: velocity in 7 bits, a note-on at least 1 so
///   that it is not read as a note-off, no note id. Pressure on one note is
///   polyphonic aftertouch. [`EventKind::Midi`] goes as it is.
/// - Other note expression, chokes and modulation have no MIDI 1.0 form,
///   and are counted as undeliverable, as is anything for a plugin with no
///   MIDI input.
///
/// An event at or beyond the end of a block is delivered on the first
/// frame of the next; 1,024 can wait that way.
///
/// # What the plugin sends
///
/// Every atom output is given an empty buffer of the size the plugin was
/// told (`options:sequenceSize`, at least the `rsz:minimumSize` it asks
/// for) before each call of `run`, as LV2 has a host ready an output: a
/// chunk whose size is the room there is, for the plugin to overwrite
/// with a sequence. After the call, the outputs that say they carry
/// `midi:MidiEvent` are read, and every MIDI 1.0 message the plugin wrote
/// there is in [`output_events`](Self::output_events), on its frame of the
/// caller's block, however many pieces the block was run in. Other atoms
/// in those sequences, and the other outputs, are not read.
pub struct Lv2Processor {
    shared: Arc<Shared>,
    layout: Arc<Layout>,
    slots: Arc<[ParamSlot]>,
    sample_rate: f64,
    max_frames: usize,
    urids: Urids,
    /// One buffer per main and then sidechain audio input.
    inputs: Vec<Box<[f32]>>,
    /// One buffer per main and then other audio output.
    outputs: Vec<Box<[f32]>>,
    /// One buffer per CV port.
    cv: Vec<Box<[f32]>>,
    /// One value per port, by index; only control ports' are used.
    controls: Box<[f32]>,
    events: Option<AtomBuffer>,
    atom_inputs: Vec<AtomBuffer>,
    atom_outputs: Vec<AtomBuffer>,
    /// The ports of each parameter, by its place in the list.
    param_ports: Box<[u32]>,
    from_main: Option<Consumer<ToAudio>>,
    carry: Vec<Event>,
    /// What the plugin sent during the last block.
    sent: Sent,
    wrong_rate: bool,
    lost: u64,
    undeliverable: u64,
}

// SAFETY: the instance is shared with the other half through `Shared`,
// whose calls are kept apart by LV2's threading classes, the instance's
// lock and `&mut self` here; everything else is owned memory.
unsafe impl Send for Lv2Processor {}

impl Lv2Processor {
    pub(crate) fn new(setup: Setup<'_>) -> Self {
        let layout = setup.layout;
        let frames = setup.max_frames;
        let buffer = || vec![0.0f32; frames].into_boxed_slice();
        let mut controls = vec![0.0f32; layout.port_count].into_boxed_slice();
        for &(port, value) in &layout.controls {
            if let Some(slot) = controls.get_mut(port as usize) {
                *slot = value;
            }
        }
        let param_ports: Box<[u32]> = setup.params.iter().map(|param| param.port).collect();
        for (&port, &value) in param_ports.iter().zip(setup.values) {
            if let Some(slot) = controls.get_mut(port as usize) {
                *slot = value;
            }
        }
        let atoms = |count: usize| {
            (0..count)
                .map(|_| AtomBuffer::new(layout.atom_bytes))
                .collect::<Vec<_>>()
        };
        let urids = Urids::new();
        let mut processor = Self {
            inputs: (0..layout.main_inputs.len() + layout.sidechain_inputs.len())
                .map(|_| buffer())
                .collect(),
            outputs: (0..layout.main_outputs.len()
                + layout.other_outputs.len()
                + layout
                    .aux_outputs
                    .iter()
                    .map(|bus| bus.ports.len())
                    .sum::<usize>())
                .map(|_| buffer())
                .collect(),
            cv: (0..layout.cv.len()).map(|_| buffer()).collect(),
            controls,
            events: layout.events.map(|_| AtomBuffer::new(layout.atom_bytes)),
            atom_inputs: atoms(layout.atom_inputs.len()),
            atom_outputs: atoms(layout.atom_outputs.len()),
            param_ports,
            from_main: Some(setup.from_main),
            carry: Vec::with_capacity(CARRIED_EVENTS),
            sent: Sent::new(EVENTS_PER_BLOCK),
            wrong_rate: false,
            lost: 0,
            undeliverable: 0,
            urids,
            slots: setup.slots,
            sample_rate: setup.sample_rate,
            max_frames: frames,
            layout,
            shared: setup.shared,
        };
        // Every port is connected before the instance is activated, and
        // again before every call of `run`.
        processor.connect();
        processor.empty_atoms();
        let instance = &processor.shared.instance;
        {
            let _held = instance.guard();
            if let Some(activate) = instance.table.activate {
                // SAFETY: the instance is alive and not active, every port
                // is connected, and the lock keeps the state functions
                // away.
                unsafe { activate(instance.handle) };
            }
        }
        processor
            .shared
            .processor_alive
            .store(true, Ordering::Release);
        processor
    }

    /// Processes one block as [`process_with`](Processor::process_with)
    /// does, and copies what the plugin wrote on its auxiliary output buses,
    /// those of [`Lv2Plugin::aux_outputs`](super::Lv2Plugin::aux_outputs),
    /// into `aux`: one buffer per bus, in their order.
    ///
    /// Each buffer gets as many channels as both it and its bus have, on
    /// the frames of `buffer`; anything else in it is left as it was. A bus
    /// without a buffer is not copied, and one switched off is silent.
    /// Real-time safe.
    pub fn process_with_aux(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        context: &ProcessContext<'_, f32>,
        aux: &mut [AudioBuffer<f32>],
    ) {
        let _quiet = NoDenormals::new();
        self.run(buffer, context, aux);
    }

    /// The sample rate the plugin runs at.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// The longest piece the plugin is run for.
    pub fn max_block_size(&self) -> usize {
        self.max_frames
    }

    /// What the plugin sent during the last call to
    /// [`process_with`](Processor::process_with), on the frames of the
    /// block it was sent on, in the order it was sent.
    ///
    /// LV2 has a plugin send MIDI 1.0, so a note comes with a velocity in 7
    /// bits and no id, and pressure on one key as a note expression;
    /// controllers, the wheel, programs and channel pressure are
    /// [`EventKind::Midi`]. System messages are left out. At most
    /// [`EVENTS_PER_BLOCK`] are kept for one block; more are dropped and
    /// counted in [`Lv2Plugin::lost_events`](super::Lv2Plugin::lost_events).
    /// What the plugin could not fit into its buffer it drops itself, and
    /// no host can count. Real-time safe.
    pub fn output_events(&self) -> &[Event] {
        self.sent.as_slice()
    }

    /// Tells the plugin where every port's data is, with pointers taken
    /// fresh, so that none it holds outlives a borrow of what it points to.
    fn connect(&mut self) {
        let instance = &self.shared.instance;
        let Some(connect) = instance.table.connect_port else {
            return;
        };
        let handle = instance.handle;
        let layout = &*self.layout;
        // SAFETY: the instance's own function, called on the thread that
        // runs it and never while `run` is; every pointer is to memory this
        // value owns, sized for the port it is given to, and stays there
        // until the next call of this function or the instance's end.
        unsafe {
            let audio_in = layout.main_inputs.iter().chain(&layout.sidechain_inputs);
            for (&port, buffer) in audio_in.zip(&mut self.inputs) {
                connect(handle, port, buffer.as_mut_ptr().cast());
            }
            // An auxiliary bus switched off is left unconnected.
            let audio_out = layout
                .main_outputs
                .iter()
                .chain(&layout.other_outputs)
                .map(|&port| (port, true))
                .chain(
                    layout
                        .aux_outputs
                        .iter()
                        .flat_map(|bus| bus.ports.iter().map(|&port| (port, bus.connected))),
                );
            for ((port, connected), buffer) in audio_out.zip(&mut self.outputs) {
                let data = if connected {
                    buffer.as_mut_ptr().cast()
                } else {
                    std::ptr::null_mut()
                };
                connect(handle, port, data);
            }
            for (&port, buffer) in layout.cv.iter().zip(&mut self.cv) {
                connect(handle, port, buffer.as_mut_ptr().cast());
            }
            let controls = self.controls.as_mut_ptr();
            for &(port, _) in &layout.controls {
                if (port as usize) < self.controls.len() {
                    connect(handle, port, controls.add(port as usize).cast());
                }
            }
            if let (Some(port), Some(buffer)) = (layout.events, &mut self.events) {
                connect(handle, port, buffer.as_mut_ptr());
            }
            for (&port, buffer) in layout.atom_inputs.iter().zip(&mut self.atom_inputs) {
                connect(handle, port, buffer.as_mut_ptr());
            }
            for (&port, buffer) in layout.atom_outputs.iter().zip(&mut self.atom_outputs) {
                connect(handle, port, buffer.as_mut_ptr());
            }
            for &port in &layout.unconnected {
                connect(handle, port, std::ptr::null_mut());
            }
        }
    }

    /// Empty sequences in every atom input and room in every output.
    fn empty_atoms(&mut self) {
        let urids = self.urids;
        if let Some(events) = &mut self.events {
            events.sequence(&urids);
        }
        for buffer in &mut self.atom_inputs {
            buffer.sequence(&urids);
        }
        for buffer in &mut self.atom_outputs {
            buffer.prepare_output(&urids);
        }
    }

    /// Stores what the other half sent.
    fn take_from_main(&mut self) {
        while let Some(message) = self.from_main.as_mut().and_then(Consumer::pop) {
            let ToAudio::Param { port, value } = message;
            if let Some(slot) = self.controls.get_mut(port as usize) {
                *slot = value;
            }
        }
    }

    /// Stores a parameter value carried by an event, or counts it.
    fn apply(&mut self, kind: &EventKind) {
        let EventKind::ParamValue { param, value } = *kind else {
            return;
        };
        let target = find_param(&self.slots, param).filter(|_| !value.is_nan());
        match target {
            Some(slot) => {
                if let Some(control) = self.controls.get_mut(slot.port as usize) {
                    *control = slot.hold(value);
                }
            }
            None => self.undeliverable += 1,
        }
    }

    /// Keeps events for the first frame of the next block.
    fn keep(&mut self, events: &[Event]) {
        for event in events {
            if self.carry.len() < CARRIED_EVENTS {
                self.carry.push(*event);
            } else {
                self.lost += 1;
            }
        }
    }

    /// Reads the MIDI the plugin wrote into its outputs during the piece
    /// that starts `start` frames into the block and is `frames` long.
    fn read_outputs(&mut self, start: usize, frames: usize) {
        let urids = self.urids;
        let outputs = self.atom_outputs.iter().zip(&self.layout.midi_outputs);
        for (buffer, _) in outputs.filter(|(_, midi)| **midi) {
            buffer.read_events(&urids, |frame, type_, body| {
                if type_ == urids.midi_event {
                    self.sent.midi(start, frames, frame, body);
                }
            });
        }
    }

    /// Passes the counts and the values on to the other half.
    fn publish(&mut self) {
        self.lost += self.sent.take_lost();
        let shared = &self.shared;
        if self.lost > 0 {
            shared.lost_events.fetch_add(self.lost, Ordering::Relaxed);
            self.lost = 0;
        }
        if self.undeliverable > 0 {
            shared
                .undeliverable_events
                .fetch_add(self.undeliverable, Ordering::Relaxed);
            self.undeliverable = 0;
        }
        for (value, &port) in shared.values.iter().zip(self.param_ports.iter()) {
            if let Some(&control) = self.controls.get(port as usize) {
                value.store(control.to_bits(), Ordering::Relaxed);
            }
        }
        if let Some(port) = self.layout.latency {
            if let Some(&frames) = self.controls.get(port as usize) {
                shared
                    .latency
                    .store(clamp_latency(frames), Ordering::Relaxed);
            }
        }
    }

    fn run(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        context: &ProcessContext<'_, f32>,
        aux: &mut [AudioBuffer<f32>],
    ) {
        self.sent.clear();
        self.take_from_main();
        let events = context.events.as_slice();
        if self.wrong_rate {
            buffer.clear();
            for bus in aux.iter_mut() {
                bus.clear();
            }
            self.keep(events);
            self.shared.failed_blocks.fetch_add(1, Ordering::Relaxed);
            self.shared.wrong_rate.store(true, Ordering::Relaxed);
            self.publish();
            return;
        }
        let total = buffer.num_frames();
        // A sidechain shorter than the block cannot be lined up with it,
        // and is treated as not connected, for the whole block.
        let sidechain = context
            .sidechain
            .filter(|sidechain| sidechain.num_frames() >= total);
        let mut transport = *context.transport;
        let mut next = 0;
        let mut start = 0;
        while start < total {
            let mut end = (start + self.max_frames).min(total);
            if start == 0 {
                // What waited from the last block goes first.
                for index in 0..self.carry.len() {
                    let kind = self.carry[index].kind;
                    self.apply(&kind);
                }
            }
            // The parameter changes that hold from this frame on are
            // stored, and the piece ends where the next one is.
            let first = next;
            while let Some(event) = events.get(next) {
                let offset = event.offset as usize;
                if offset >= end {
                    break;
                }
                if matches!(event.kind, EventKind::ParamValue { .. }) {
                    if offset > start {
                        end = offset;
                        break;
                    }
                    self.apply(&event.kind);
                }
                next += 1;
            }
            self.write_events(&events[first..next], start, &transport);
            if start == 0 {
                self.carry.clear();
            }
            let urids = self.urids;
            for buffer in &mut self.atom_inputs {
                buffer.sequence(&urids);
            }
            for buffer in &mut self.atom_outputs {
                buffer.prepare_output(&urids);
            }

            let frames = end - start;
            self.load(buffer, sidechain, start, frames);
            self.connect();
            let instance = &self.shared.instance;
            if let Some(run) = instance.table.run {
                // SAFETY: the instance is active, every port was connected
                // just now to buffers of at least `frames` frames, and
                // nothing else calls `run` or `connect_port`, which belong
                // to the thread that holds this value.
                unsafe { run(instance.handle, frames as u32) };
            }
            self.read_outputs(start, frames);
            self.store(buffer, start, frames);
            self.store_aux(aux, start, frames);

            start = end;
            if start < total {
                advance(&mut transport, frames, self.sample_rate);
            }
        }
        self.keep(events.get(next..).unwrap_or_default());
        self.publish();
    }

    /// Writes the event input for the piece that starts at `start`: the
    /// transport, then what waited from the last block, then the piece's
    /// MIDI, each on its frame within the piece.
    fn write_events(&mut self, piece: &[Event], start: usize, transport: &Transport) {
        let carried = if start == 0 { self.carry.len() } else { 0 };
        let not_values = |event: &&Event| !matches!(event.kind, EventKind::ParamValue { .. });
        let urids = self.urids;
        let (takes_time, takes_midi) = (self.layout.events_take_time, self.layout.events_take_midi);
        let Some(input) = &mut self.events else {
            let waiting = self.carry[..carried].iter().chain(piece);
            self.undeliverable += waiting.filter(not_values).count() as u64;
            return;
        };
        let mut sequence = input.sequence(&urids);
        if takes_time && !sequence.position(0, transport, &urids) {
            self.lost += 1;
        }
        let waiting = self.carry[..carried].iter().map(|event| (0, event));
        let own = piece
            .iter()
            .map(|event| ((event.offset as usize).saturating_sub(start) as u32, event));
        for (frame, event) in waiting.chain(own) {
            if !not_values(&event) {
                continue;
            }
            match midi_of(&event.kind).filter(|_| takes_midi) {
                Some(message) => {
                    if !sequence.midi(frame, message.to_bytes().as_slice(), &urids) {
                        self.lost += 1;
                    }
                }
                None => self.undeliverable += 1,
            }
        }
    }

    /// Copies frames `start..start + frames` of the caller's block and
    /// sidechain into the plugin's inputs, and silences its outputs.
    fn load(
        &mut self,
        buffer: &AudioBuffer<f32>,
        sidechain: Option<&AudioBuffer<f32>>,
        start: usize,
        frames: usize,
    ) {
        let end = start + frames;
        let main = self.layout.main_inputs.len();
        for (index, input) in self.inputs.iter_mut().enumerate() {
            let (source, channel) = if index < main {
                (Some(buffer), index)
            } else {
                (sidechain, index - main)
            };
            let samples = source
                .filter(|source| channel < source.num_channels())
                .and_then(|source| source.channel(channel).get(start..end));
            match samples {
                Some(samples) => input[..frames].copy_from_slice(samples),
                None => input[..frames].fill(0.0),
            }
        }
        for output in &mut self.outputs {
            output[..frames].fill(0.0);
        }
        for cv in &mut self.cv {
            cv[..frames].fill(0.0);
        }
    }

    /// Copies the plugin's auxiliary output buses, in their order, into
    /// frames `start..start + frames` of `aux`, one buffer per bus; a bus
    /// switched off is silent. Channels a buffer has and its bus lacks, and
    /// frames past a buffer's end, are left as they were.
    fn store_aux(&self, aux: &mut [AudioBuffer<f32>], start: usize, frames: usize) {
        let mut first = self.layout.main_outputs.len() + self.layout.other_outputs.len();
        for (bus, target) in self.layout.aux_outputs.iter().zip(aux.iter_mut()) {
            let outputs = &self.outputs[first..first + bus.ports.len()];
            first += bus.ports.len();
            for (channel, target) in target.iter_channels_mut().enumerate() {
                let Some(target) = target.get_mut(start..start + frames) else {
                    continue;
                };
                match outputs.get(channel).filter(|_| bus.connected) {
                    Some(output) => target.copy_from_slice(&output[..frames]),
                    None => target.fill(0.0),
                }
            }
        }
    }

    /// Copies the plugin's main outputs into frames `start..start + frames`
    /// of the caller's block.
    fn store(&self, buffer: &mut AudioBuffer<f32>, start: usize, frames: usize) {
        let main = self.layout.main_outputs.len();
        for (channel, target) in buffer.iter_channels_mut().enumerate() {
            let target = &mut target[start..start + frames];
            match self.outputs.get(channel).filter(|_| channel < main) {
                Some(output) => target.copy_from_slice(&output[..frames]),
                None => target.fill(0.0),
            }
        }
    }
}

impl Processor<f32> for Lv2Processor {
    /// Checks that the plugin can run under `spec`. Its sample rate was
    /// fixed when it was created; if `spec` asks for another, every block
    /// is silent until this is called again with the plugin's rate. Any
    /// block size and channel count are accepted. Does not allocate.
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.wrong_rate = spec.sample_rate != self.sample_rate;
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        self.process_with(buffer, &ProcessContext::empty());
    }

    fn process_with(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.process_with_aux(buffer, context, &mut []);
    }

    /// Deactivates and activates the instance, which clears its voices and
    /// tails, and forgets the notes kept for the next block. Parameter
    /// values kept for the next block are still delivered.
    fn reset(&mut self) {
        self.carry
            .retain(|event| matches!(event.kind, EventKind::ParamValue { .. }));
        let instance = &self.shared.instance;
        // Never waits: if the other half holds the lock, it is saving the
        // state, and the reset is skipped.
        let Ok(_held) = instance.lock.try_lock() else {
            return;
        };
        // SAFETY: the instance is active and not running, since this value
        // is what runs it, and the lock keeps the state functions away.
        unsafe {
            if let Some(deactivate) = instance.table.deactivate {
                deactivate(instance.handle);
            }
            if let Some(activate) = instance.table.activate {
                activate(instance.handle);
            }
        }
    }

    /// The latency the plugin's latency port last reported, in frames.
    fn latency_samples(&self) -> usize {
        self.shared.latency.load(Ordering::Relaxed) as usize
    }
}

impl Drop for Lv2Processor {
    fn drop(&mut self) {
        // Changes the audio thread never read are kept, so none is lost.
        self.take_from_main();
        self.from_main = None;
        let instance = &self.shared.instance;
        {
            let _held = instance.guard();
            if let Some(deactivate) = instance.table.deactivate {
                // SAFETY: the instance is active and, with this value going,
                // nothing runs it any more; the lock keeps the state
                // functions away.
                unsafe { deactivate(instance.handle) };
            }
        }
        self.publish();
        self.shared.processor_alive.store(false, Ordering::Release);
    }
}

impl std::fmt::Debug for Lv2Processor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lv2Processor")
            .field("sample_rate", &self.sample_rate)
            .field("max_block_size", &self.max_frames)
            .finish_non_exhaustive()
    }
}
