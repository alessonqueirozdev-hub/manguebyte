//! The audio half of a hosted Audio Unit.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use manguebyte_core::{AudioBuffer, NoDenormals};
use manguebyte_dsp::event::{Event, EventKind};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};

use super::events::{Call, ParamSlot, immediate, translate};
use super::plugin::Shared;
use super::sys::AudioBuffer as AuBuffer;
use super::sys::*;
use super::transport::Answers;
use super::{AuError, CARRIED_EVENTS, EVENTS_PER_BLOCK, refused};
use crate::delivery::{Sent, advance};
use crate::threads::guard;

/// Everything [`AuProcessor::new`] needs.
pub(crate) struct Setup {
    pub(crate) shared: Arc<Shared>,
    pub(crate) slots: Arc<[ParamSlot]>,
    pub(crate) sample_rate: f64,
    /// The longest slice the unit renders.
    pub(crate) slice: usize,
    /// For the main input and the sidechain: whether the unit has the
    /// element, and its channels.
    pub(crate) inputs: [(bool, usize); 2],
    /// The output element's channels.
    pub(crate) outputs: usize,
}

/// What the render callback hands a unit: the audio of the slice for each
/// input element.
///
/// It lives on the heap at an address that does not move, which the unit
/// holds as the callback's data. The processor writes it only between
/// renders, and the callback reads it only during one, both through the
/// raw pointer, so the two never hold a reference at the same time.
struct Feed {
    /// One buffer per channel, per input element.
    elements: [Vec<Box<[f32]>>; 2],
    /// How many frames of each buffer the slice holds.
    frames: usize,
}

/// Where the MIDI a unit sends during a render goes: its
/// `kAudioUnitProperty_MIDIOutputCallback` writes here.
///
/// Like the [`Feed`], it lives on the heap at an address that does not
/// move, which the unit holds as the callback's data. The processor writes
/// it only between renders and the callback only during one, which Apple
/// has a unit call it from, on the thread that renders.
struct MidiSink {
    sent: Sent,
    /// Where the slice being rendered starts within the caller's block.
    base: usize,
    /// How long that slice is.
    frames: usize,
}

/// A hosted Audio Unit as the audio thread deals with it: a [`Processor`],
/// made by [`AuPlugin::activate`](super::AuPlugin::activate).
///
/// # Real-time safety
///
/// [`process_with`](Processor::process_with) does not allocate, lock or
/// block in this crate's code: the buffers, the buffer list, the input's
/// memory and the list of carried events are allocated at activation, and
/// the transport reaches the unit's callbacks through atomics. What the
/// unit does in its own render is up to it. Subnormal numbers are flushed
/// to zero while it renders.
///
/// [`reset`](Processor::reset) calls `AudioUnitReset`, which clears the
/// unit's voices and tails: what the unit does there is up to it.
/// Dropping the processor is not real-time safe: it uninitialises the
/// unit and frees the buffers.
///
/// # How a block is laid out
///
/// The buffer is the unit's main input on the way in and its output on
/// the way out, one channel per channel of the element, in order; the
/// sidechain element reads [`ProcessContext::sidechain`]. Channels the unit
/// does not have are silent, and channels the buffer does not have are
/// discarded. The unit is fed its input through a render callback and
/// renders into memory of the processor's own, which is then copied into
/// the buffer, so it never works in place.
///
/// # Slices and events, on their frames
///
/// A block longer than the longest slice the unit takes is rendered in
/// slices of that length. Before each slice the processor schedules its
/// events, each on its frame within the slice and in the order they came:
/// parameter changes through `AudioUnitScheduleParameters` as immediate
/// changes, and notes and MIDI through `MusicDeviceMIDIEvent`. It then
/// writes the transport at the slice's start for the host callbacks and
/// calls `AudioUnitRender`. A unit whose sound does not depend on how a
/// host cuts the timeline therefore sounds the same, to the bit, however
/// the blocks fall.
///
/// - [`EventKind::ParamValue`] is in the parameter's own unit, held to its
///   range, as a 32-bit float.
/// - Notes become MIDI 1.0: velocity in 7 bits, a note-on at least 1 so
///   that it is not read as a note-off, no note id. Pressure on one note is
///   polyphonic aftertouch. [`EventKind::Midi`] goes as it is.
/// - Other note expression, chokes and modulation have no MIDI 1.0 form,
///   and are counted as undeliverable, as are notes for a unit that takes
///   no MIDI, values for parameters it does not have, and events it
///   refuses.
///
/// An event at or beyond the end of a block is delivered on the first
/// frame of the next; 1,024 can wait that way. A slice the unit fails to
/// render is silent, and counted.
///
/// # What the unit sends
///
/// A unit that has `kAudioUnitProperty_MIDIOutputCallbackInfo` is given a
/// `kAudioUnitProperty_MIDIOutputCallback` before it is initialised. The
/// `MIDIPacketList`s it hands that callback while it renders, each
/// packet's time stamp the frame of the render it falls on, are read into
/// [`output_events`](Self::output_events), on their frames of the caller's
/// block, however many slices the block was rendered in. Every MIDI output
/// the unit names goes into the one list.
pub struct AuProcessor {
    shared: Arc<Shared>,
    slots: Arc<[ParamSlot]>,
    sample_rate: f64,
    slice: usize,
    /// The input's memory; see [`Feed`]. Freed when this value goes.
    feed: *mut Feed,
    has_element: [bool; 2],
    /// What the unit renders into.
    output: AudioBuffer<f32>,
    /// An `AudioBufferList` with one buffer per output channel, in 8-byte
    /// words.
    list: Box<[u64]>,
    takes_midi: bool,
    /// What the unit sends; see [`MidiSink`]. Freed when this value goes.
    midi: *mut MidiSink,
    /// Whether the unit took the MIDI output callback.
    sends_midi: bool,
    carry: Vec<Event>,
    /// The frame of the next slice, counted from activation, for the time
    /// stamp.
    sample_time: f64,
    /// Whether the last slice was playing, and the frame the next one
    /// should then start on, to tell the unit of a jump.
    previous: Option<(bool, Option<i64>)>,
    wrong_rate: bool,
    lost: u64,
    undeliverable: u64,
}

// SAFETY: the unit is shared with the other half through `Shared`, whose
// calls are kept apart by Apple's conventions, which the two halves keep
// to; the feed and the MIDI sink are owned memory only this value and the
// unit's render, on this value's thread, reach.
unsafe impl Send for AuProcessor {}

/// The render callback of every input element.
///
/// # Safety
///
/// `user` must be the processor's [`Feed`], and `list` the unit's buffer
/// list for `frames` frames, as `AURenderCallback` gives them.
unsafe extern "C" fn feed_input(
    user: *mut c_void,
    _flags: *mut AudioUnitRenderActionFlags,
    _timestamp: *const AudioTimeStamp,
    element: u32,
    frames: u32,
    list: *mut AudioBufferList,
) -> OSStatus {
    guard(kAudioUnitErr_NoConnection, || {
        let feed = user.cast::<Feed>();
        if feed.is_null() || list.is_null() {
            return paramErr;
        }
        // SAFETY: the processor's feed, which it does not touch while the
        // unit renders, and the unit's list, with as many buffers as it
        // says.
        unsafe {
            let Some(channels) = (*feed).elements.get_mut(element as usize) else {
                return kAudioUnitErr_InvalidElement;
            };
            let available = (*feed).frames;
            let count = (*list).mNumberBuffers as usize;
            let first = list
                .cast::<u8>()
                .add(AUDIO_BUFFER_LIST_BUFFERS)
                .cast::<AuBuffer>();
            for index in 0..count {
                let buffer = &mut *first.add(index);
                if buffer.mNumberChannels > 1 {
                    return kAudioUnitErr_FormatNotSupported;
                }
                let ours = channels.get_mut(index);
                if buffer.mData.is_null() {
                    // The unit asks to be lent the host's memory.
                    match ours {
                        Some(samples) if frames as usize <= samples.len() => {
                            samples[available.min(frames as usize)..frames as usize].fill(0.0);
                            buffer.mData = samples.as_mut_ptr().cast();
                            buffer.mDataByteSize = frames * 4;
                            buffer.mNumberChannels = 1;
                        }
                        _ => return kAudioUnitErr_TooManyFramesToProcess,
                    }
                    continue;
                }
                let room = (buffer.mDataByteSize as usize / 4).min(frames as usize);
                let target = std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), room);
                let given = ours.map_or(0, |samples| {
                    let given = available.min(room).min(samples.len());
                    target[..given].copy_from_slice(&samples[..given]);
                    given
                });
                target[given..].fill(0.0);
            }
        }
        noErr
    })
}

/// The MIDI output callback: reads a `MIDIPacketList` the unit sent, laid
/// out as CoreMIDI's headers declare it (see `MIDI_PACKET_LIST_PACKETS`),
/// each packet stamped with its frame in the render.
///
/// # Safety
///
/// `user` must be the processor's [`MidiSink`], and `list` a whole
/// `MIDIPacketList`, as `AUMIDIOutputCallback` gives them, during a render.
unsafe extern "C" fn take_midi(
    user: *mut c_void,
    _timestamp: *const AudioTimeStamp,
    _output: u32,
    list: *const c_void,
) -> OSStatus {
    guard(kAudioUnitErr_CannotDoInCurrentContext, || {
        let sink = user.cast::<MidiSink>();
        if sink.is_null() || list.is_null() {
            return paramErr;
        }
        // SAFETY: the processor's sink, which it does not touch while the
        // unit renders, and the unit's list, whose packets are as long as
        // they say.
        unsafe {
            let sink = &mut *sink;
            let bytes = list.cast::<u8>();
            let count = bytes.cast::<u32>().read_unaligned();
            let mut at = MIDI_PACKET_LIST_PACKETS;
            for _ in 0..count {
                let packet = bytes.add(at);
                let time = packet.cast::<u64>().read_unaligned();
                let length = packet
                    .add(MIDI_PACKET_LENGTH)
                    .cast::<u16>()
                    .read_unaligned() as usize;
                let data = std::slice::from_raw_parts(packet.add(MIDI_PACKET_DATA), length);
                let frame = i64::try_from(time).unwrap_or(i64::MAX);
                sink.sent.midi(sink.base, sink.frames, frame, data);
                at = (at + MIDI_PACKET_DATA + length).next_multiple_of(MIDI_PACKET_ALIGN);
            }
        }
        noErr
    })
}

impl AuProcessor {
    pub(crate) fn new(setup: Setup) -> Result<Self, AuError> {
        let slice = setup.slice.max(1);
        let channels = |(has, count): (bool, usize)| {
            (0..if has { count } else { 0 })
                .map(|_| vec![0.0f32; slice].into_boxed_slice())
                .collect::<Vec<_>>()
        };
        let feed = Box::into_raw(Box::new(Feed {
            elements: [channels(setup.inputs[0]), channels(setup.inputs[1])],
            frames: 0,
        }));
        let midi = Box::into_raw(Box::new(MidiSink {
            sent: Sent::new(EVENTS_PER_BLOCK),
            base: 0,
            frames: 0,
        }));
        let words = (AUDIO_BUFFER_LIST_BUFFERS + setup.outputs * size_of::<AuBuffer>()).div_ceil(8);
        let mut processor = Self {
            midi,
            sends_midi: false,
            takes_midi: setup.shared.unit.takes_midi(),
            shared: setup.shared,
            slots: setup.slots,
            sample_rate: setup.sample_rate,
            slice,
            feed,
            has_element: [setup.inputs[0].0, setup.inputs[1].0],
            output: AudioBuffer::new(setup.outputs, slice),
            list: vec![0u64; words.max(2)].into_boxed_slice(),
            carry: Vec::with_capacity(CARRIED_EVENTS),
            sample_time: 0.0,
            previous: None,
            wrong_rate: false,
            lost: 0,
            undeliverable: 0,
        };
        // The callbacks go in before the unit is initialised; should it
        // refuse, dropping the processor takes them out again.
        for element in 0..2 {
            if !processor.has_element[element] {
                continue;
            }
            let callback = AURenderCallbackStruct {
                inputProc: Some(feed_input),
                inputProcRefCon: feed.cast(),
            };
            processor
                .shared
                .unit
                .set(
                    kAudioUnitProperty_SetRenderCallback,
                    kAudioUnitScope_Input,
                    element as u32,
                    &callback,
                )
                .map_err(refused("an input's render callback"))?;
        }
        // A unit that names MIDI outputs takes a callback for what it sends
        // on them. One that refuses it sends nothing, which is no reason
        // not to host it.
        let unit = &processor.shared.unit;
        if unit
            .property_info(
                kAudioUnitProperty_MIDIOutputCallbackInfo,
                kAudioUnitScope_Global,
                0,
            )
            .is_ok()
        {
            let callback = AUMIDIOutputCallbackStruct {
                midiOutputCallback: Some(take_midi),
                userData: midi.cast(),
            };
            processor.sends_midi = unit
                .set(
                    kAudioUnitProperty_MIDIOutputCallback,
                    kAudioUnitScope_Global,
                    0,
                    &callback,
                )
                .is_ok();
        }
        processor
            .shared
            .unit
            .initialize()
            .map_err(refused("AudioUnitInitialize"))?;
        processor
            .shared
            .processor_alive
            .store(true, Ordering::Release);
        Ok(processor)
    }

    /// The sample rate the unit renders at.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// The longest slice the unit renders.
    pub fn max_block_size(&self) -> usize {
        self.slice
    }

    /// What the unit sent through its MIDI output callback during the last
    /// call to [`process_with`](Processor::process_with), on the frames of
    /// the block it was sent on, in the order it was sent.
    ///
    /// The callback carries MIDI 1.0, so a note comes with a velocity in 7
    /// bits and no id, and pressure on one key as a note expression;
    /// controllers, the wheel, programs and channel pressure are
    /// [`EventKind::Midi`]. System messages are left out. At most
    /// [`EVENTS_PER_BLOCK`](super::EVENTS_PER_BLOCK) are kept for one
    /// block; more are dropped and counted in
    /// [`AuPlugin::lost_events`](super::AuPlugin::lost_events). Empty for
    /// a unit with no MIDI output. Real-time safe.
    pub fn output_events(&self) -> &[Event] {
        // SAFETY: the unit renders only within `process_with`, which takes
        // `&mut self`, so nothing writes the sink while this borrow lasts.
        unsafe { (*self.midi).sent.as_slice() }
    }

    /// The sink, between renders.
    fn sink(&mut self) -> &mut MidiSink {
        // SAFETY: the unit is not rendering, so nothing else reaches the
        // sink; the reference lasts no longer than this borrow of `self`.
        unsafe { &mut *self.midi }
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

    /// Hands the unit one event, on `frame` of the slice.
    fn deliver(&mut self, kind: &EventKind, frame: u32) {
        let unit = &self.shared.unit;
        let delivered = match translate(kind, &self.slots, self.takes_midi) {
            Call::Param { id, value } => unit.schedule(&immediate(id, frame, value)).is_ok(),
            Call::Midi(bytes) => unit.midi(bytes, frame).is_ok(),
            Call::Undeliverable => false,
        };
        if !delivered {
            self.undeliverable += 1;
        }
    }

    /// Passes the counts on to the other half.
    fn publish(&mut self) {
        self.lost += self.sink().sent.take_lost();
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
    }

    /// Copies frames `start..start + frames` of the block and the sidechain
    /// into the feed.
    fn load(
        &mut self,
        buffer: &AudioBuffer<f32>,
        sidechain: Option<&AudioBuffer<f32>>,
        start: usize,
        frames: usize,
    ) {
        let end = start + frames;
        // SAFETY: the unit is not rendering, so nothing else reaches the
        // feed; the reference ends with this function.
        let feed = unsafe { &mut *self.feed };
        feed.frames = frames;
        for (element, source) in [Some(buffer), sidechain].into_iter().enumerate() {
            for (channel, samples) in feed.elements[element].iter_mut().enumerate() {
                let given = source
                    .filter(|source| channel < source.num_channels())
                    .and_then(|source| source.channel(channel).get(start..end));
                match given {
                    Some(given) => samples[..frames].copy_from_slice(given),
                    None => samples[..frames].fill(0.0),
                }
            }
        }
    }

    /// The transport for the slice, with whether it moved other than by
    /// playing on, and what to expect of the next.
    fn answers(&mut self, transport: &Transport, frames: usize) -> Answers {
        let changed = match self.previous {
            None => true,
            Some((playing, expected)) => {
                playing != transport.playing
                    || (transport.playing && expected != transport.position_samples)
            }
        };
        let next = transport
            .position_samples
            .map(|frame| frame.saturating_add(i64::try_from(frames).unwrap_or(i64::MAX)));
        self.previous = Some((
            transport.playing,
            if transport.playing {
                next
            } else {
                transport.position_samples
            },
        ));
        Answers {
            transport: *transport,
            sample_rate: self.sample_rate,
            changed,
        }
    }

    /// Renders one slice into `buffer`'s frames `start..start + frames`.
    /// Returns whether the unit rendered it.
    fn render_slice(&mut self, buffer: &mut AudioBuffer<f32>, start: usize, frames: usize) -> bool {
        let channels = self.output.num_channels();
        let list = self.list.as_mut_ptr().cast::<AudioBufferList>();
        // SAFETY: the words hold the header and a buffer per channel.
        let buffers = unsafe {
            (*list).mNumberBuffers = channels as u32;
            std::slice::from_raw_parts_mut(
                list.cast::<u8>()
                    .add(AUDIO_BUFFER_LIST_BUFFERS)
                    .cast::<AuBuffer>(),
                channels,
            )
        };
        for (slot, samples) in buffers.iter_mut().zip(self.output.iter_channels_mut()) {
            samples[..frames].fill(0.0);
            *slot = AuBuffer {
                mNumberChannels: 1,
                mDataByteSize: (frames * 4) as u32,
                mData: samples.as_mut_ptr().cast(),
            };
        }
        let mut flags: AudioUnitRenderActionFlags = 0;
        let stamp = AudioTimeStamp {
            mSampleTime: self.sample_time,
            mFlags: kAudioTimeStampSampleTimeValid,
            ..AudioTimeStamp::default()
        };
        // SAFETY: the list holds a buffer of `frames` frames per output
        // channel, in this value's memory, and the unit was initialised
        // for slices of up to `self.slice` frames, which `frames` is not
        // above.
        let status = unsafe {
            self.shared
                .unit
                .render(&mut flags, &stamp, frames as u32, list)
        };
        self.sample_time += frames as f64;
        let end = start + frames;
        if let Err(status) = status {
            self.shared.last_error.store(status, Ordering::Relaxed);
            for channel in buffer.iter_channels_mut() {
                channel[start..end].fill(0.0);
            }
            return false;
        }
        // The unit may have pointed the buffers at its own memory.
        // SAFETY: the list as the unit left it, each buffer's memory valid
        // until its next render.
        let given = unsafe {
            std::slice::from_raw_parts(
                list.cast::<u8>()
                    .add(AUDIO_BUFFER_LIST_BUFFERS)
                    .cast::<AuBuffer>(),
                ((*list).mNumberBuffers as usize).min(channels),
            )
        };
        for (channel, target) in buffer.iter_channels_mut().enumerate() {
            let target = &mut target[start..end];
            let source = given
                .get(channel)
                .filter(|slot| !slot.mData.is_null() && slot.mNumberChannels == 1);
            match source {
                Some(slot) => {
                    let length = (slot.mDataByteSize as usize / 4).min(frames);
                    // SAFETY: the buffer holds `mDataByteSize` bytes.
                    let samples =
                        unsafe { std::slice::from_raw_parts(slot.mData.cast::<f32>(), length) };
                    target[..length].copy_from_slice(samples);
                    target[length..].fill(0.0);
                }
                None => target.fill(0.0),
            }
        }
        true
    }

    fn run(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.sink().sent.clear();
        let events = context.events.as_slice();
        if self.wrong_rate {
            buffer.clear();
            self.keep(events);
            self.shared.failed_blocks.fetch_add(1, Ordering::Relaxed);
            self.shared.wrong_rate.store(true, Ordering::Relaxed);
            self.publish();
            return;
        }
        let total = buffer.num_frames();
        // A sidechain shorter than the block cannot be lined up with it,
        // and is treated as silent, for the whole block.
        let sidechain = context
            .sidechain
            .filter(|sidechain| sidechain.num_frames() >= total);
        let mut transport = *context.transport;
        let mut next = 0;
        let mut start = 0;
        let mut failed = false;
        while start < total {
            let frames = self.slice.min(total - start);
            let end = start + frames;
            if start == 0 {
                // What waited from the last block goes first.
                for index in 0..self.carry.len() {
                    let kind = self.carry[index].kind;
                    self.deliver(&kind, 0);
                }
                self.carry.clear();
            }
            while let Some(event) = events.get(next) {
                let offset = event.offset as usize;
                if offset >= end {
                    break;
                }
                let kind = event.kind;
                self.deliver(&kind, offset.saturating_sub(start) as u32);
                next += 1;
            }
            self.load(buffer, sidechain, start, frames);
            let answers = self.answers(&transport, frames);
            self.shared.transport.write(&answers);
            let sink = self.sink();
            sink.base = start;
            sink.frames = frames;
            if !self.render_slice(buffer, start, frames) {
                failed = true;
            }
            start = end;
            if start < total {
                advance(&mut transport, frames, self.sample_rate);
            }
        }
        if failed {
            self.shared.failed_blocks.fetch_add(1, Ordering::Relaxed);
        }
        self.keep(events.get(next..).unwrap_or_default());
        self.publish();
    }
}

impl Processor<f32> for AuProcessor {
    /// Checks that the unit can render under `spec`. Its sample rate was
    /// set when it was activated; if `spec` asks for another, every block
    /// is silent until this is called again with the unit's rate. Any
    /// block size and channel count are accepted. Does not allocate.
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.wrong_rate = spec.sample_rate != self.sample_rate;
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        self.process_with(buffer, &ProcessContext::empty());
    }

    fn process_with(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        let _quiet = NoDenormals::new();
        self.run(buffer, context);
    }

    /// Calls `AudioUnitReset`, which clears the unit's voices and tails,
    /// and forgets the notes kept for the next block. Parameter values
    /// kept for the next block are still delivered.
    fn reset(&mut self) {
        self.carry
            .retain(|event| matches!(event.kind, EventKind::ParamValue { .. }));
        let _ = self.shared.unit.reset();
        self.previous = None;
    }

    /// The latency the unit reported, in frames.
    fn latency_samples(&self) -> usize {
        self.shared.latency.load(Ordering::Relaxed) as usize
    }
}

impl Drop for AuProcessor {
    fn drop(&mut self) {
        let unit = &self.shared.unit;
        let _ = unit.uninitialize();
        // The unit must not keep a callback whose data is about to go.
        let none = AURenderCallbackStruct {
            inputProc: None,
            inputProcRefCon: ptr::null_mut(),
        };
        for element in 0..2 {
            if self.has_element[element] {
                let _ = unit.set(
                    kAudioUnitProperty_SetRenderCallback,
                    kAudioUnitScope_Input,
                    element as u32,
                    &none,
                );
            }
        }
        if self.sends_midi {
            let none = AUMIDIOutputCallbackStruct {
                midiOutputCallback: None,
                userData: ptr::null_mut(),
            };
            let _ = unit.set(
                kAudioUnitProperty_MIDIOutputCallback,
                kAudioUnitScope_Global,
                0,
                &none,
            );
        }
        // The transport of a stopped host, for a unit that asks later.
        self.shared.transport.write(&Answers {
            transport: Transport::STOPPED,
            sample_rate: self.sample_rate,
            changed: true,
        });
        // SAFETY: the feed came from `Box::into_raw`, and no unit holds it
        // any more.
        drop(unsafe { Box::from_raw(self.feed) });
        // What did not fit in the last block is counted before the sink
        // goes.
        self.publish();
        // SAFETY: the sink came from `Box::into_raw`, and no unit holds it
        // any more: the callback was taken out above, or the unit refused
        // it.
        drop(unsafe { Box::from_raw(self.midi) });
        self.shared.processor_alive.store(false, Ordering::Release);
    }
}

impl std::fmt::Debug for AuProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuProcessor")
            .field("sample_rate", &self.sample_rate)
            .field("max_block_size", &self.slice)
            .finish_non_exhaustive()
    }
}
