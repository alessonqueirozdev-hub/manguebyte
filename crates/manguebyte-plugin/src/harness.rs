use std::sync::Arc;

use manguebyte_core::{AudioBuffer, NoDenormals};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
use manguebyte_dsp::note_input::NoteInput;
use manguebyte_dsp::timeline::{TempoMap, TimeSignature, TransportClock};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Transport};
use manguebyte_midi::MidiMessage;
use manguebyte_midi::ump::Packet;
use manguebyte_params::{LoadReport, ParamSet};

use crate::editor::EditorSpec;
use crate::output::{OutputBuffer, OutputKind};
use crate::{
    AudioLayout, AuxOutputs, ParamWatcher, Plugin, StateError, apply_param_events, load_state,
    save_state,
};

/// Most events the harness will deliver in a single block.
const EVENTS_PER_BLOCK: usize = 4_096;

/// Runs a plugin without a host, for tests and offline rendering.
///
/// The harness does what a host does: it prepares the plugin, cuts time into
/// blocks, delivers notes and automation on their frames, keeps a transport
/// moving and saves and restores state. Everything is deterministic, so a
/// plugin's sound can be checked in an ordinary unit test and in CI, on
/// machines with no sound card.
///
/// Events are scheduled by their position on the timeline, in frames from
/// the start, and are delivered when rendering reaches them.
///
/// The transport comes from a [`TransportClock`] playing a [`TempoMap`]:
/// 120 bpm in 4/4 unless [`with_tempo`](Self::with_tempo) or
/// [`with_tempo_map`](Self::with_tempo_map) says otherwise, playing from
/// the start. A map with tempo changes, ramps and time signatures is how a
/// test automates the tempo: the plugin sees the transport at the start of
/// each block and, in [`ProcessContext::timeline`], the map and where each
/// of its changes falls in the block. [`play`](Self::play),
/// [`stop`](Self::stop), [`seek_beat`](Self::seek_beat) and
/// [`set_loop_beats`](Self::set_loop_beats) work the clock between renders;
/// scheduled events stay on the frames rendered, whatever the clock does.
///
/// See the [crate documentation](crate) for an example.
pub struct Harness<P: Plugin> {
    plugin: P,
    params: Arc<P::Params>,
    /// Taken from the plugin once, as a wrapper does.
    editor: Option<Box<dyn EditorSpec>>,
    /// The layout the plugin runs with.
    layout: AudioLayout,
    sample_rate: f64,
    max_block_size: usize,
    /// Block lengths to cycle through.
    pattern: Vec<usize>,
    next_in_pattern: usize,
    /// Events not yet delivered, sorted by frame.
    scheduled: Vec<(u64, Scheduled)>,
    events: EventBuffer,
    /// Reads scheduled MIDI as a wrapper reads a host's, following
    /// [`Plugin::MPE`].
    input: NoteInput,
    /// The stretch of the sidechain that goes with the block being rendered.
    sidechain: AudioBuffer<f32>,
    watcher: ParamWatcher,
    clock: TransportClock,
    /// The transport the next block will start with.
    transport: Transport,
    position: u64,
    /// What the plugin sends during a block, with room for a block's worth
    /// if it says it sends anything.
    output: OutputBuffer,
    /// What it has sent since the last `take_output`, on the timeline.
    sent: Vec<(u64, OutputKind)>,
    /// The auxiliary output buses of the block being rendered.
    aux: AuxOutputs,
    /// Each auxiliary bus as the last render wrote it, whole; `None` for
    /// one that was switched off.
    aux_rendered: Vec<Option<AudioBuffer<f32>>>,
}

impl<P: Plugin> Harness<P> {
    /// Creates the plugin and prepares it.
    ///
    /// # Panics
    ///
    /// Panics if `max_block_size` is zero, if `sample_rate` is not a
    /// positive, finite number, or if the plugin is not well formed: its id
    /// is empty or contains spaces, it has no outputs, or its parameters
    /// fail [`ParamSet::validate`]. A plugin that cannot pass these checks
    /// would misbehave in a real host too.
    pub fn new(sample_rate: f64, max_block_size: usize) -> Self {
        assert!(max_block_size > 0, "max_block_size must be non-zero");
        let mut clock = TransportClock::new(TempoMap::default(), sample_rate);
        clock.play();
        let id = crate::plugin_info::<P>().id;
        assert!(
            !id.is_empty() && !id.contains(char::is_whitespace),
            "plugin id {id:?} must be non-empty and free of spaces"
        );
        assert!(P::LAYOUT.outputs > 0, "a plugin needs at least one output");

        let mut plugin = P::new();
        let params = plugin.params();
        if let Err(error) = params.validate() {
            panic!("plugin {id} has invalid parameters: {error}");
        }
        let editor = plugin.editor();
        plugin.set_layout(P::LAYOUT);
        plugin.prepare(&ProcessSpec {
            sample_rate,
            max_block_size,
            num_channels: P::LAYOUT.outputs,
        });

        Self {
            plugin,
            layout: P::LAYOUT,
            sample_rate,
            max_block_size,
            pattern: vec![max_block_size],
            next_in_pattern: 0,
            scheduled: Vec::new(),
            events: EventBuffer::with_capacity(EVENTS_PER_BLOCK),
            input: NoteInput::new(P::MPE),
            sidechain: AudioBuffer::new(P::LAYOUT.sidechain, max_block_size),
            watcher: ParamWatcher::new(&*params),
            params,
            editor,
            transport: clock.transport(),
            clock,
            position: 0,
            output: if P::NOTE_OUTPUT {
                OutputBuffer::with_capacity(EVENTS_PER_BLOCK)
            } else {
                OutputBuffer::new()
            },
            sent: Vec::new(),
            aux: AuxOutputs::for_plugin::<P>(max_block_size),
            aux_rendered: vec![None; P::AUX_OUTPUTS.len()],
        }
    }

    /// Sets the block lengths the harness cycles through, to imitate a host
    /// that does not use one fixed size. Lengths above the maximum block
    /// size are cut down to it.
    ///
    /// # Panics
    ///
    /// Panics if `pattern` is empty or contains a zero.
    #[must_use]
    pub fn with_block_pattern(mut self, pattern: &[usize]) -> Self {
        assert!(
            !pattern.is_empty() && !pattern.contains(&0),
            "a block pattern needs at least one non-zero length"
        );
        self.pattern = pattern.to_vec();
        self.next_in_pattern = 0;
        self
    }

    /// Runs the plugin with `layout` instead of [`Plugin::LAYOUT`], as a
    /// host that chose it would: [`Plugin::set_layout`] and then
    /// [`Plugin::prepare`] again. See [`set_layout`](Self::set_layout).
    ///
    /// # Panics
    ///
    /// Panics if the wrappers would not offer `layout`: it is not among
    /// [`offered_layouts`](crate::offered_layouts), or has no outputs.
    #[must_use]
    pub fn with_layout(mut self, layout: AudioLayout) -> Self {
        self.set_layout(layout);
        self
    }

    /// Switches the plugin to `layout` between renders, as a host does
    /// while audio is stopped: [`Plugin::set_layout`], then
    /// [`Plugin::prepare`] with the layout's outputs. Renders then have its
    /// channel counts, and a sidechain is passed on only if it has one.
    ///
    /// # Panics
    ///
    /// As for [`with_layout`](Self::with_layout).
    pub fn set_layout(&mut self, layout: AudioLayout) {
        assert!(
            layout.outputs > 0 && crate::offered_layouts::<P>().contains(&layout),
            "plugin {} does not offer the layout {layout:?}",
            crate::plugin_info::<P>().id
        );
        self.layout = layout;
        self.sidechain = AudioBuffer::new(layout.sidechain, self.max_block_size);
        self.plugin.set_layout(layout);
        self.plugin.prepare(&ProcessSpec {
            sample_rate: self.sample_rate,
            max_block_size: self.max_block_size,
            num_channels: layout.outputs,
        });
    }

    /// The layout the plugin runs with.
    pub fn layout(&self) -> AudioLayout {
        self.layout
    }

    /// Plays one tempo throughout, in 4/4, in place of the tempo map.
    ///
    /// # Panics
    ///
    /// Panics if `bpm` is not a positive, finite number.
    #[must_use]
    pub fn with_tempo(self, bpm: f64) -> Self {
        let map = TempoMap::new(bpm, TimeSignature::FOUR_FOUR)
            .unwrap_or_else(|error| panic!("tempo {bpm}: {error}"));
        self.with_tempo_map(map)
    }

    /// Plays `map`: its tempo changes, ramps and time signatures reach the
    /// plugin on their frames.
    #[must_use]
    pub fn with_tempo_map(mut self, map: TempoMap) -> Self {
        self.set_tempo_map(map);
        self
    }

    /// Plays `map` from the next block, keeping the musical position as
    /// [`TransportClock::replace_map`] does, and returns the map it
    /// replaces.
    pub fn set_tempo_map(&mut self, map: TempoMap) -> TempoMap {
        let old = self.clock.replace_map(Box::new(map));
        self.transport = self.clock.transport();
        *old
    }

    /// The tempo map being played.
    pub fn tempo_map(&self) -> &TempoMap {
        self.clock.map()
    }

    /// The clock behind the transport, to read where it is.
    pub fn transport_clock(&self) -> &TransportClock {
        &self.clock
    }

    /// Starts the transport from where it is. It plays from the start
    /// unless stopped.
    pub fn play(&mut self) {
        self.clock.play();
        self.transport = self.clock.transport();
    }

    /// Stops the transport where it is. Rendering goes on, and the plugin
    /// sees a stopped transport.
    pub fn stop(&mut self) {
        self.clock.stop();
        self.transport = self.clock.transport();
    }

    /// Sets whether the transport says it records.
    pub fn set_recording(&mut self, recording: bool) {
        self.clock.set_recording(recording);
        self.transport = self.clock.transport();
    }

    /// Moves the transport to the frame `beat` lands on.
    pub fn seek_beat(&mut self, beat: f64) {
        self.clock.seek_beat(beat);
        self.transport = self.clock.transport();
    }

    /// Moves the transport to `frame` of the musical timeline.
    pub fn seek_frame(&mut self, frame: i64) {
        self.clock.seek_frame(frame);
        self.transport = self.clock.transport();
    }

    /// Loops the transport between two positions in quarter notes, or
    /// stops looping with `None`.
    pub fn set_loop_beats(&mut self, range: Option<(f64, f64)>) {
        self.clock.set_loop_beats(range);
        self.transport = self.clock.transport();
    }

    /// The plugin's parameters.
    pub fn params(&self) -> &P::Params {
        &self.params
    }

    /// How to open the plugin's editor, if it has one.
    ///
    /// The harness asks the plugin once, when it creates it, as a wrapper
    /// does, so a test can open the editor with a
    /// [`RecordingEditorHost`](crate::editor::testing::RecordingEditorHost)
    /// while the plugin processes.
    pub fn editor(&self) -> Option<&dyn EditorSpec> {
        self.editor.as_deref()
    }

    /// The plugin itself.
    pub fn plugin_mut(&mut self) -> &mut P {
        &mut self.plugin
    }

    /// The transport as the plugin will next see it.
    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// How many frames have been rendered so far. Events are scheduled on
    /// this count; the transport's position moves with it while it plays,
    /// and can be seeked and looped apart from it.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Everything the plugin has sent out since this was last called, from
    /// [`Plugin::process_with_output`], each on its frame of the timeline
    /// as [`position`](Self::position) counts them. An event sent past the
    /// end of its block is on the block's last frame, as a host would have
    /// it. Empty for a plugin that does not set [`Plugin::NOTE_OUTPUT`].
    ///
    /// The frames do not depend on the block sizes the plugin was given,
    /// so comparing what two [block patterns](Self::with_block_pattern)
    /// send checks that a plugin's output does not either.
    pub fn take_output(&mut self) -> Vec<(u64, OutputKind)> {
        std::mem::take(&mut self.sent)
    }

    /// Switches auxiliary output bus `index` of [`Plugin::AUX_OUTPUTS`] on
    /// or off from the next render, as a host does while audio is
    /// stopped. Each starts as its
    /// [`active_by_default`](crate::AuxOutput::active_by_default) says.
    ///
    /// # Panics
    ///
    /// Panics if the plugin has no such bus.
    pub fn set_aux_active(&mut self, index: usize, active: bool) {
        assert!(
            index < self.aux.len(),
            "plugin {} has {} auxiliary outputs, not {}",
            crate::plugin_info::<P>().id,
            self.aux.len(),
            index + 1
        );
        self.aux.set_active(index, active);
    }

    /// Auxiliary output bus `index` as the last [`render`](Self::render)
    /// or [`process`](Self::process) wrote it, as long as that call, or
    /// `None` if the bus was switched off then or there is no such bus.
    pub fn aux_output(&self, index: usize) -> Option<&AudioBuffer<f32>> {
        self.aux_rendered.get(index)?.as_ref()
    }

    /// Schedules an event at `frame` on the timeline.
    ///
    /// An event scheduled for a frame that has already been rendered is
    /// delivered at the start of the next block.
    pub fn schedule(&mut self, frame: u64, kind: EventKind) {
        self.schedule_item(frame, Scheduled::Event(kind));
    }

    /// Schedules a MIDI 1.0 message at `frame`, read as the plugin
    /// wrappers read a host's MIDI: through a
    /// [`NoteInput`](manguebyte_dsp::note_input::NoteInput) that follows
    /// [`Plugin::MPE`], so that MPE arrives as per-note events. A message
    /// passed to [`schedule`](Self::schedule) as [`EventKind::Midi`]
    /// reaches the plugin as it is instead.
    pub fn schedule_midi(&mut self, frame: u64, message: MidiMessage) {
        self.schedule_item(frame, Scheduled::Midi(message));
    }

    /// Schedules a Universal MIDI Packet at `frame`, read as
    /// [`schedule_midi`](Self::schedule_midi) reads MIDI 1.0: MIDI 2.0
    /// notes and per-note messages arrive as per-note events.
    pub fn schedule_ump(&mut self, frame: u64, packet: Packet) {
        self.schedule_item(frame, Scheduled::Packet(packet));
    }

    fn schedule_item(&mut self, frame: u64, item: Scheduled) {
        let index = self.scheduled.partition_point(|&(at, _)| at <= frame);
        self.scheduled.insert(index, (frame, item));
    }

    /// Schedules a note on channel 0 that starts at `start` and is released
    /// `length` frames later.
    pub fn note(&mut self, start: u64, length: u64, key: u8, velocity: f32) {
        let note = NoteRef::new(0, key);
        self.schedule(start, EventKind::NoteOn { note, velocity });
        self.schedule(
            start + length,
            EventKind::NoteOff {
                note,
                velocity: 0.0,
            },
        );
    }

    /// Schedules a parameter to take `value`, in its own unit, at `frame`.
    ///
    /// # Panics
    ///
    /// Panics if the plugin has no parameter with this identifier.
    pub fn automate(&mut self, frame: u64, id: &str, value: f64) {
        let param = self
            .params
            .find(id)
            .unwrap_or_else(|| {
                panic!(
                    "plugin {} has no parameter {id:?}",
                    crate::plugin_info::<P>().id
                )
            })
            .param_id();
        self.schedule(frame, EventKind::ParamValue { param, value });
    }

    /// Renders `frames` frames with silence at the input. This is how an
    /// instrument is played.
    pub fn render(&mut self, frames: usize) -> AudioBuffer<f32> {
        let mut output = AudioBuffer::new(self.layout.outputs, frames);
        self.run(&mut output, None);
        output
    }

    /// Passes `input` through the plugin and returns the result. This is how
    /// an effect is used.
    ///
    /// Input channels beyond the plugin's own input count are ignored, and
    /// missing ones are treated as silent. The plugin sees no sidechain, as
    /// in a host where nothing is routed to it.
    pub fn process(&mut self, input: &AudioBuffer<f32>) -> AudioBuffer<f32> {
        self.process_through(input, None)
    }

    /// Passes `input` through the plugin while `sidechain` plays on its
    /// sidechain input, and returns the result.
    ///
    /// As with the input, extra sidechain channels are ignored and missing
    /// ones are silent.
    ///
    /// # Panics
    ///
    /// Panics if the two buffers differ in length.
    pub fn process_with_sidechain(
        &mut self,
        input: &AudioBuffer<f32>,
        sidechain: &AudioBuffer<f32>,
    ) -> AudioBuffer<f32> {
        assert_eq!(
            input.num_frames(),
            sidechain.num_frames(),
            "the input and the sidechain must have the same length"
        );
        self.process_through(input, Some(sidechain))
    }

    fn process_through(
        &mut self,
        input: &AudioBuffer<f32>,
        sidechain: Option<&AudioBuffer<f32>>,
    ) -> AudioBuffer<f32> {
        let mut output = AudioBuffer::new(self.layout.outputs, input.num_frames());
        let shared = self
            .layout
            .inputs
            .min(self.layout.outputs)
            .min(input.num_channels());
        for channel in 0..shared {
            output
                .channel_mut(channel)
                .copy_from_slice(input.channel(channel));
        }
        self.run(&mut output, sidechain);
        output
    }

    /// Captures the plugin's state as text.
    pub fn save_state(&self) -> String {
        save_state::<P>(&self.params)
    }

    /// Restores state captured by [`save_state`](Self::save_state).
    pub fn load_state(&self, text: &str) -> Result<LoadReport, StateError> {
        load_state::<P>(&self.params, text)
    }

    /// Clears the plugin's tails and filter memory, as a host does when
    /// playback jumps.
    pub fn reset(&mut self) {
        self.plugin.reset();
    }

    fn run(&mut self, buffer: &mut AudioBuffer<f32>, sidechain: Option<&AudioBuffer<f32>>) {
        // The same arithmetic as inside a host, where the wrappers do this.
        let _quiet = NoDenormals::new();
        let frames = buffer.num_frames();
        for (index, rendered) in self.aux_rendered.iter_mut().enumerate() {
            *rendered = self
                .aux
                .is_active(index)
                .then(|| AudioBuffer::new(P::AUX_OUTPUTS[index].channels, frames));
        }
        let mut done = 0;
        while done < frames {
            let wanted = self.pattern[self.next_in_pattern];
            self.next_in_pattern = (self.next_in_pattern + 1) % self.pattern.len();
            let length = wanted.min(self.max_block_size).min(frames - done);

            // Changes made behind the host's back first, then the host's
            // own events, so that everything reaches the plugin in one stream.
            self.events.clear();
            self.watcher.emit_changes(&*self.params, &mut self.events);
            self.gather_events(length);
            apply_param_events(&*self.params, &self.events);
            self.watcher.remember(&*self.params);
            let block = self.clock.next_block(length);
            let map = self.clock.map();
            let transport = block.transport(map);
            let mut context =
                ProcessContext::new(&self.events, &transport).with_timeline(block.with_map(map));
            if let Some(whole) = sidechain.filter(|_| self.layout.sidechain > 0) {
                // The plugin gets the stretch that goes with this block.
                self.sidechain.set_num_frames(length);
                self.sidechain.clear();
                let shared = whole.num_channels().min(self.sidechain.num_channels());
                for channel in 0..shared {
                    self.sidechain
                        .channel_mut(channel)
                        .copy_from_slice(&whole.channel(channel)[done..done + length]);
                }
                context = context.with_sidechain(&self.sidechain);
            }
            let plugin = &mut self.plugin;
            let output = &mut self.output;
            let aux = &mut self.aux;
            output.clear();
            aux.start_block(length);
            buffer.windowed(done..done + length, |block| {
                plugin.process_with_aux(block, &context, output, aux);
            });
            for (index, rendered) in self.aux_rendered.iter_mut().enumerate() {
                if let (Some(whole), Some(bus)) = (rendered.as_mut(), self.aux.bus(index)) {
                    for (channel, samples) in bus.iter_channels().enumerate() {
                        whole.channel_mut(channel)[done..done + length].copy_from_slice(samples);
                    }
                }
            }
            let last = length.saturating_sub(1) as u64;
            for event in &self.output {
                let frame = self.position + u64::from(event.offset).min(last);
                self.sent.push((frame, event.kind));
            }

            self.transport = self.clock.transport();
            self.position += length as u64;
            done += length;
        }
    }

    /// Moves the scheduled events that fall inside the next block into the
    /// event buffer.
    fn gather_events(&mut self, length: usize) {
        let end = self.position + length as u64;
        let due = self.scheduled.partition_point(|&(at, _)| at < end);
        let Self {
            events,
            input,
            scheduled,
            position,
            ..
        } = self;
        let mut push = |event| {
            events
                .push(event)
                .expect("more events in one block than the harness can deliver");
        };
        for (at, item) in scheduled.drain(..due) {
            let offset = at.saturating_sub(*position) as u32;
            match item {
                Scheduled::Event(kind) => push(Event { offset, kind }),
                Scheduled::Midi(message) => input.midi1(offset, message, &mut push),
                Scheduled::Packet(packet) => input.packet(offset, &packet, &mut push),
            }
        }
    }
}

/// Something waiting to be delivered.
enum Scheduled {
    Event(EventKind),
    Midi(MidiMessage),
    Packet(Packet),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioLayout, PluginInfo, PluginKind};
    use manguebyte_dsp::event::Expression;
    use manguebyte_dsp::{Synth, Voice, split_at_events};
    use manguebyte_params::{BoolParam, FloatParam, FloatRange, ParamId, param_set};

    // --- An effect that follows automation sample by sample. ---

    struct LevelParams {
        level: FloatParam,
        mute: BoolParam,
    }
    param_set!(LevelParams { level, mute });

    const LEVEL: ParamId = ParamId::from_name("level");

    struct Level {
        params: Arc<LevelParams>,
        /// The level in effect on the frame being rendered.
        current: f32,
        prepared_for: Option<ProcessSpec>,
    }

    impl Plugin for Level {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.level",
            name: "Level",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Scales the input",
            kind: PluginKind::Effect,
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
        type Params = LevelParams;

        fn new() -> Self {
            let params = Arc::new(LevelParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 4.0), 1.0),
                mute: BoolParam::new("mute", "Mute", false),
            });
            Self {
                current: params.level.value(),
                params,
                prepared_for: None,
            }
        }

        fn params(&self) -> Arc<LevelParams> {
            Arc::clone(&self.params)
        }

        fn prepare(&mut self, spec: &ProcessSpec) {
            self.prepared_for = Some(*spec);
        }

        fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
            let muted = self.params.mute.value();
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
                |current, piece| piece.apply_gain(if muted { 0.0 } else { *current }),
            );
        }
    }

    fn ones(channels: usize, frames: usize) -> AudioBuffer<f32> {
        let mut buffer = AudioBuffer::new(channels, frames);
        for channel in buffer.iter_channels_mut() {
            channel.fill(1.0);
        }
        buffer
    }

    #[test]
    fn prepares_the_plugin_for_its_own_layout() {
        let mut harness = Harness::<Level>::new(44_100.0, 128);
        assert_eq!(
            harness.plugin_mut().prepared_for,
            Some(ProcessSpec {
                sample_rate: 44_100.0,
                max_block_size: 128,
                num_channels: 2,
            })
        );
    }

    #[test]
    fn automation_lands_on_its_exact_frame_and_updates_the_parameter() {
        let mut harness = Harness::<Level>::new(48_000.0, 64);
        harness.automate(100, "level", 2.0);
        harness.automate(101, "level", 3.0);
        harness.automate(250, "level", 0.5);

        let output = harness.process(&ones(2, 300));
        let left = output.channel(0);
        assert_eq!(&left[..100], &[1.0; 100]);
        assert_eq!(left[100], 2.0);
        assert_eq!(&left[101..250], &[3.0; 149]);
        assert_eq!(&left[250..], &[0.5; 50]);
        assert_eq!(output.channel(1), left);

        // Anything polling the parameter sees where automation left it.
        assert_eq!(harness.params().level.value(), 0.5);
        assert_eq!(harness.position(), 300);
    }

    #[test]
    fn the_output_does_not_depend_on_the_block_pattern() {
        let render = |max: usize, pattern: &[usize]| {
            let mut harness = Harness::<Level>::new(48_000.0, max).with_block_pattern(pattern);
            for (frame, value) in [(0, 0.25), (63, 1.5), (64, 2.5), (65, 0.1), (777, 3.0)] {
                harness.automate(frame, "level", value);
            }
            harness.process(&ones(2, 1_000))
        };
        let reference = render(64, &[64]);
        for (max, pattern) in [(1, &[1][..]), (1_000, &[1_000]), (128, &[3, 97, 128, 1])] {
            assert!(render(max, pattern) == reference, "pattern {pattern:?}");
        }
    }

    #[test]
    fn late_events_are_delivered_at_the_next_block() {
        let mut harness = Harness::<Level>::new(48_000.0, 32);
        harness.process(&ones(2, 100));
        harness.automate(10, "level", 2.0); // already in the past
        let output = harness.process(&ones(2, 4));
        assert_eq!(output.channel(0), &[2.0; 4]);
    }

    #[test]
    fn changes_made_outside_the_host_reach_the_plugin_as_events() {
        // `Level` tracks its value from events alone and never polls, so
        // this only passes if direct writes are turned into events.
        let mut harness = Harness::<Level>::new(48_000.0, 32);
        harness.params().level.set(4.0);
        assert_eq!(harness.process(&ones(2, 8)).channel(0), &[4.0; 8]);

        // A restored state is such a change too.
        let saved = harness.save_state().replace("level=4", "level=0.25");
        harness.load_state(&saved).unwrap();
        assert_eq!(harness.process(&ones(2, 8)).channel(0), &[0.25; 8]);

        // Host automation on frame 0 of the same block takes precedence.
        harness.params().level.set(3.0);
        harness.automate(harness.position(), "level", 2.0);
        assert_eq!(harness.process(&ones(2, 8)).channel(0), &[2.0; 8]);
        assert_eq!(harness.params().level.value(), 2.0);

        harness.params().mute.set(true);
        assert_eq!(harness.process(&ones(2, 8)).peak(), 0.0);
    }

    #[test]
    fn extra_input_channels_are_ignored_and_missing_ones_are_silent() {
        let mut harness = Harness::<Level>::new(48_000.0, 32);
        let output = harness.process(&ones(5, 8));
        assert_eq!(output.num_channels(), 2);
        assert_eq!(output.channel(1), &[1.0; 8]);

        let output = harness.process(&ones(1, 8));
        assert_eq!(output.channel(0), &[1.0; 8]);
        assert_eq!(output.channel(1), &[0.0; 8]);
    }

    #[test]
    fn state_round_trips_and_other_plugins_state_is_refused() {
        let harness = Harness::<Level>::new(48_000.0, 32);
        harness.params().level.set(2.5);
        harness.params().mute.set(true);
        let saved = harness.save_state();
        assert!(saved.starts_with("manguebyte-plugin 1 org.manguebyte.test.level\n"));

        let fresh = Harness::<Level>::new(48_000.0, 32);
        let report = fresh.load_state(&saved).unwrap();
        assert!(report.is_exact());
        assert_eq!(fresh.params().level.value(), 2.5);
        assert!(fresh.params().mute.value());

        let foreign = saved.replace("test.level", "test.other");
        assert_eq!(
            fresh.load_state(&foreign),
            Err(StateError::WrongPlugin(String::from(
                "org.manguebyte.test.other"
            )))
        );
        assert_eq!(fresh.load_state("hello"), Err(StateError::NotPluginState));
        assert_eq!(
            fresh.load_state("manguebyte-plugin 9 org.manguebyte.test.level\n"),
            Err(StateError::UnsupportedVersion(9))
        );
        assert!(matches!(
            fresh.load_state("manguebyte-plugin 1 org.manguebyte.test.level\ngarbage"),
            Err(StateError::Params(_))
        ));
        // A refused load changes nothing.
        assert_eq!(fresh.params().level.value(), 2.5);
    }

    #[test]
    fn the_transport_moves_with_the_audio() {
        let mut harness = Harness::<Level>::new(48_000.0, 512).with_tempo(60.0);
        harness.process(&ones(2, 48_000));
        let beats = harness.transport().position_beats.unwrap();
        assert!((beats - 1.0).abs() < 1e-9);
        assert_eq!(harness.transport().position_samples, Some(48_000));
        assert_eq!(harness.transport().tempo_bpm, Some(60.0));
        assert_eq!(harness.transport().position_seconds, Some(1.0));
        assert_eq!(harness.transport().bar_number, Some(0));
        assert_eq!(
            harness.tempo_map().signature_at_beat(0.0),
            TimeSignature::FOUR_FOUR
        );
    }

    #[test]
    #[should_panic(expected = "tempo 0")]
    fn a_tempo_of_zero_is_a_mistake() {
        let _ = Harness::<Level>::new(48_000.0, 16).with_tempo(0.0);
    }

    // --- An instrument. ---

    struct NoParams {}
    param_set!(NoParams {});

    /// Outputs the key number, scaled by pressure, while the key is down.
    #[derive(Default)]
    struct KeyVoice {
        level: f32,
        pressure: f32,
    }

    impl Voice for KeyVoice {
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn note_on(&mut self, note: NoteRef, _: f32) {
            self.level = f32::from(note.key);
            self.pressure = 1.0;
        }
        fn note_off(&mut self, _: f32) {
            self.level = 0.0;
        }
        fn choke(&mut self) {
            self.level = 0.0;
        }
        fn is_active(&self) -> bool {
            self.level > 0.0
        }
        fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
            for channel in buffer.iter_channels_mut() {
                for sample in channel {
                    *sample += self.level * self.pressure;
                }
            }
        }
        fn expression(&mut self, expression: Expression, value: f64) {
            if expression == Expression::Pressure {
                self.pressure = value as f32;
            }
        }
    }

    struct Keys {
        synth: Synth<KeyVoice>,
        params: Arc<NoParams>,
    }

    impl Plugin for Keys {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.keys",
            name: "Keys",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Plays key numbers",
            kind: PluginKind::Instrument,
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
        type Params = NoParams;

        fn new() -> Self {
            Self {
                synth: Synth::with_voices(4, KeyVoice::default),
                params: Arc::new(NoParams {}),
            }
        }
        fn params(&self) -> Arc<NoParams> {
            Arc::clone(&self.params)
        }
        fn prepare(&mut self, spec: &ProcessSpec) {
            use manguebyte_dsp::Processor;
            self.synth.prepare(spec);
        }
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
            use manguebyte_dsp::Processor;
            self.synth.process_with(buffer, context);
        }
        fn reset(&mut self) {
            use manguebyte_dsp::Processor;
            self.synth.reset();
        }
    }

    #[test]
    fn an_instrument_plays_scheduled_notes_with_per_note_expression() {
        let mut harness = Harness::<Keys>::new(48_000.0, 16).with_block_pattern(&[7, 16, 3]);
        harness.note(10, 30, 60, 1.0);
        harness.note(20, 30, 64, 1.0);
        harness.schedule(
            30,
            EventKind::NoteExpression {
                note: NoteRef::new(0, 64),
                expression: Expression::Pressure,
                value: 0.5,
            },
        );

        let output = harness.render(60);
        let left = output.channel(0);
        assert_eq!(&left[..10], &[0.0; 10]);
        assert_eq!(&left[10..20], &[60.0; 10]);
        assert_eq!(&left[20..30], &[124.0; 10]);
        // Pressure halves the upper note only.
        assert_eq!(&left[30..40], &[92.0; 10]);
        assert_eq!(&left[40..50], &[32.0; 10]);
        assert_eq!(&left[50..], &[0.0; 10]);
        assert_eq!(output.channel(1), left);

        harness.note(60, 1_000, 72, 1.0);
        assert_eq!(harness.render(4).channel(0), &[72.0; 4]);
        harness.reset();
        assert_eq!(harness.render(4).peak(), 0.0);
    }

    /// `Keys`, made to be played from an MPE controller.
    struct MpeKeys(Keys);

    impl Plugin for MpeKeys {
        const INFO: PluginInfo = Keys::INFO;
        const LAYOUT: AudioLayout = Keys::LAYOUT;
        const MPE: crate::Mpe = crate::Mpe::LOWER_ZONE;
        type Params = NoParams;

        fn new() -> Self {
            Self(Keys::new())
        }
        fn params(&self) -> Arc<NoParams> {
            self.0.params()
        }
        fn prepare(&mut self, spec: &ProcessSpec) {
            self.0.prepare(spec);
        }
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
            self.0.process(buffer, context);
        }
    }

    #[test]
    fn mpe_and_midi_2_reach_an_instrument_as_per_note_events() {
        use manguebyte_midi::translate::scale_7_to_32;
        use manguebyte_midi::ump::{ChannelVoice2, Message, NoteAttribute};

        let on = |channel, note| MidiMessage::NoteOn {
            channel,
            note,
            velocity: 100,
        };
        let off = |channel, note| MidiMessage::NoteOff {
            channel,
            note,
            velocity: 0,
        };
        // Two notes on member channels; the second one's channel pressure
        // halves it, and the first one's does nothing to it.
        let mut mpe = Harness::<MpeKeys>::new(48_000.0, 16).with_block_pattern(&[5, 16]);
        mpe.schedule_midi(10, on(1, 60));
        mpe.schedule_midi(20, on(2, 64));
        mpe.schedule_midi(
            30,
            MidiMessage::ChannelPressure {
                channel: 2,
                pressure: 64,
            },
        );
        mpe.schedule_midi(40, off(1, 60));
        mpe.schedule_midi(50, off(2, 64));
        let from_mpe = mpe.render(60);
        let left = from_mpe.channel(0);
        assert_eq!(&left[20..30], &[124.0; 10]);
        assert!((left[35] - (60.0 + 64.0 * 0.5)).abs() < 1e-3);
        assert_eq!(&left[50..], &[0.0; 10]);

        // The same in MIDI 2.0 packets, per note on channel 1.
        let packet = |message| Message::Midi2 { group: 0, message }.to_packet();
        let on2 = |note| {
            packet(ChannelVoice2::NoteOn {
                channel: 0,
                note,
                velocity: manguebyte_midi::translate::scale_7_to_16(100),
                attribute: NoteAttribute::NONE,
            })
        };
        let off2 = |note| {
            packet(ChannelVoice2::NoteOff {
                channel: 0,
                note,
                velocity: 0,
                attribute: NoteAttribute::NONE,
            })
        };
        let mut two = Harness::<Keys>::new(48_000.0, 16).with_block_pattern(&[16, 3]);
        two.schedule_ump(10, on2(60));
        two.schedule_ump(20, on2(64));
        two.schedule_ump(
            30,
            packet(ChannelVoice2::PolyPressure {
                channel: 0,
                note: 64,
                pressure: scale_7_to_32(64),
            }),
        );
        two.schedule_ump(40, off2(60));
        two.schedule_ump(50, off2(64));
        assert_eq!(two.render(60), from_mpe);
    }

    #[test]
    #[should_panic(expected = "has no parameter \"cutoff\"")]
    fn automating_a_parameter_that_does_not_exist_is_a_mistake() {
        Harness::<Keys>::new(48_000.0, 16).automate(0, "cutoff", 1.0);
    }

    // --- An effect that adds its sidechain to its input. ---

    struct Listener {
        params: Arc<NoParams>,
    }

    impl Plugin for Listener {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.listener",
            name: "Listener",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Adds the sidechain to the input",
            kind: PluginKind::Effect,
        };
        const LAYOUT: AudioLayout = AudioLayout {
            inputs: 1,
            outputs: 1,
            sidechain: 2,
        };
        type Params = NoParams;

        fn new() -> Self {
            Self {
                params: Arc::new(NoParams {}),
            }
        }
        fn params(&self) -> Arc<NoParams> {
            Arc::clone(&self.params)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
            // With nothing routed to the sidechain, say so loudly.
            let Some(sidechain) = context.sidechain else {
                buffer.channel_mut(0).fill(-1.0);
                return;
            };
            assert_eq!(sidechain.num_frames(), buffer.num_frames());
            assert_eq!(sidechain.num_channels(), 2);
            for (frame, sample) in buffer.channel_mut(0).iter_mut().enumerate() {
                *sample += sidechain.channel(0)[frame] + sidechain.channel(1)[frame];
            }
        }
    }

    #[test]
    fn the_sidechain_reaches_the_plugin_in_step_with_the_input() {
        let ramp = |channels: usize, scale: f32| {
            let mut buffer = AudioBuffer::new(channels, 40);
            for channel in buffer.iter_channels_mut() {
                for (frame, sample) in channel.iter_mut().enumerate() {
                    *sample = frame as f32 * scale;
                }
            }
            buffer
        };
        // Blocks of uneven length, so that the sidechain has to be cut in
        // the same places as the input.
        let mut harness = Harness::<Listener>::new(48_000.0, 16).with_block_pattern(&[7, 16, 3]);
        let input = ramp(1, 1.0);

        // Two sidechain channels, each a tenth of the frame number.
        let output = harness.process_with_sidechain(&input, &ramp(2, 0.1));
        for (frame, &sample) in output.channel(0).iter().enumerate() {
            assert!((sample - frame as f32 * 1.2).abs() < 1e-4, "frame {frame}");
        }
        // A missing sidechain channel is silent, and an extra one ignored.
        let output = harness.process_with_sidechain(&input, &ramp(1, 0.1));
        assert!((output.channel(0)[30] - 33.0).abs() < 1e-4);
        let output = harness.process_with_sidechain(&input, &ramp(3, 0.1));
        assert!((output.channel(0)[30] - 36.0).abs() < 1e-4);

        // Without one, the plugin is told there is none.
        assert_eq!(harness.process(&input).channel(0), &[-1.0; 40]);
    }

    #[test]
    #[should_panic(expected = "must have the same length")]
    fn a_sidechain_of_another_length_is_a_mistake() {
        let mut harness = Harness::<Listener>::new(48_000.0, 16);
        harness.process_with_sidechain(&AudioBuffer::new(1, 8), &AudioBuffer::new(2, 9));
    }

    // --- A plugin that follows the tempo map. ---

    /// Clicks on channel 0 on every frame a quarter note lands on, and
    /// writes the tempo on channel 1 and the bar on channel 2, all read
    /// from the timeline in its context.
    struct Metronome(Arc<NoParams>);

    impl Plugin for Metronome {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.metronome",
            name: "Metronome",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Clicks on the beat",
            kind: PluginKind::Instrument,
        };
        const LAYOUT: AudioLayout = AudioLayout {
            inputs: 0,
            outputs: 3,
            sidechain: 0,
        };
        type Params = NoParams;

        fn new() -> Self {
            Self(Arc::new(NoParams {}))
        }
        fn params(&self) -> Arc<NoParams> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
            let timeline = context.timeline.expect("the harness gives a timeline");
            assert_eq!(*context.transport, timeline.transport());
            for frame in 0..buffer.num_frames() {
                let at = context.offset + frame;
                let position = timeline.position_at(at);
                let beat = position.beat.round();
                let click = timeline.block.playing
                    && timeline.map.frame_at_beat(beat, timeline.block.sample_rate)
                        == timeline.block.frame_at(at);
                buffer.channel_mut(0)[frame] = if click { 1.0 } else { 0.0 };
                buffer.channel_mut(1)[frame] = position.tempo_bpm as f32;
                buffer.channel_mut(2)[frame] = position.bar as f32;
            }
        }
    }

    fn clicks(output: &AudioBuffer<f32>) -> Vec<usize> {
        let channel = output.channel(0);
        (0..channel.len())
            .filter(|&frame| channel[frame] == 1.0)
            .collect()
    }

    /// 120 bpm, a ramp down to 60 over beats 4 to 8, then 60; 4/4, then
    /// 3/4 from bar 1 (beat 4) and 7/8 from bar 3 (beat 10).
    fn song() -> TempoMap {
        use manguebyte_dsp::timeline::TempoCurve;
        let mut map = TempoMap::new(120.0, TimeSignature::FOUR_FOUR).unwrap();
        map.set_tempo(4.0, 120.0, TempoCurve::Linear).unwrap();
        map.set_tempo(8.0, 60.0, TempoCurve::Step).unwrap();
        map.set_signature(1, TimeSignature::new(3, 4)).unwrap();
        map.set_signature(3, TimeSignature::new(7, 8)).unwrap();
        map
    }

    // Eight seconds of song four times, once a frame at a time: hours
    // under Miri, for arithmetic that has no unsafe code in it.
    #[test]
    #[cfg_attr(miri, ignore = "too long for Miri, and safe code only")]
    fn a_plugin_follows_the_tempo_map_on_its_frames() {
        const RATE: f64 = 44_100.0;
        let map = song();
        let render = |pattern: &[usize]| {
            let mut harness = Harness::<Metronome>::new(RATE, 512)
                .with_tempo_map(song())
                .with_block_pattern(pattern);
            harness.render(8 * RATE as usize)
        };
        let reference = render(&[512]);
        for pattern in [&[1usize][..], &[64, 7, 300], &[511, 2]] {
            let output = render(pattern);
            for channel in 0..3 {
                assert_eq!(
                    output.channel(channel),
                    reference.channel(channel),
                    "pattern {pattern:?}, channel {channel}"
                );
            }
        }

        // A click on every quarter note, before, through and after the
        // ramp.
        let expected: Vec<usize> = (0..14)
            .map(|beat| map.frame_at_beat(f64::from(beat), RATE) as usize)
            .filter(|&frame| frame < reference.num_frames())
            .collect();
        assert!(expected.len() > 10, "the slow part was heard");
        assert_eq!(clicks(&reference), expected);
        // The tempo slows down through the ramp, frame by frame.
        let ramp_start = map.frame_at_beat(4.0, RATE) as usize;
        let ramp_end = map.frame_at_beat(8.0, RATE) as usize;
        let tempo = reference.channel(1);
        assert_eq!(tempo[ramp_start - 1], 120.0);
        assert!(tempo[ramp_start + 1] < 120.0);
        assert!(
            tempo[ramp_start..ramp_end]
                .windows(2)
                .all(|pair| pair[1] <= pair[0])
        );
        assert_eq!(tempo[ramp_end], 60.0);
        // Each bar starts on the frame its line lands on.
        let bar = reference.channel(2);
        for (number, beat) in [(1.0, 4.0), (2.0, 7.0), (3.0, 10.0)] {
            let line = map.frame_at_beat(beat, RATE) as usize;
            assert_eq!((bar[line - 1], bar[line]), (number - 1.0, number));
        }
    }

    #[test]
    fn the_transport_can_loop_seek_and_stop_between_renders() {
        const RATE: f64 = 48_000.0;
        let mut harness = Harness::<Metronome>::new(RATE, 256).with_block_pattern(&[100]);
        // Beats 1 to 3 at 120 bpm: frames 24 000 to 72 000.
        harness.set_loop_beats(Some((1.0, 3.0)));
        assert_eq!(harness.transport().loop_beats, Some((1.0, 3.0)));
        let output = harness.render(150_000);
        assert_eq!(
            clicks(&output),
            [0, 24_000, 48_000, 72_000, 96_000, 120_000, 144_000]
        );
        assert_eq!(harness.transport_clock().position(), 54_000);
        assert_eq!(harness.position(), 150_000);

        harness.seek_beat(10.0);
        harness.set_loop_beats(None);
        let transport = *harness.transport();
        assert_eq!(transport.position_beats, Some(10.0));
        assert_eq!(transport.bar_number, Some(2));
        assert_eq!(transport.bar_start_beats, Some(8.0));
        let output = harness.render(30_000);
        assert_eq!(clicks(&output), [0, 24_000]);

        // Stopped, nothing moves and nothing clicks.
        harness.stop();
        assert!(!harness.transport().playing);
        let at = harness.transport_clock().position();
        assert!(clicks(&harness.render(30_000)).is_empty());
        assert_eq!(harness.transport_clock().position(), at);
        harness.set_recording(true);
        harness.play();
        assert!(harness.transport().playing && harness.transport().recording);

        // A new map keeps the beat, 11.25, and moves the frame.
        let beat = harness.transport().position_beats.unwrap();
        assert_eq!(beat, 11.25);
        let old = harness.set_tempo_map(TempoMap::new(60.0, TimeSignature::FOUR_FOUR).unwrap());
        assert_eq!(old, TempoMap::default());
        assert_eq!(harness.tempo_map().tempo_at_beat(0.0), 60.0);
        assert_eq!(harness.transport().position_beats, Some(11.25));
        assert_eq!(harness.transport().position_samples, Some(540_000));
        harness.seek_frame(0);
        assert_eq!(harness.transport().position_samples, Some(0));
    }

    // --- A plugin with more than one layout. ---

    /// Writes how many channels it was prepared for, plus ten when it is
    /// given a sidechain.
    struct Shaped {
        params: Arc<NoParams>,
        layouts: Vec<AudioLayout>,
        channels: usize,
    }

    impl Plugin for Shaped {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.shaped",
            name: "Shaped",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Runs mono or stereo",
            kind: PluginKind::Effect,
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT.with_sidechain(2);
        const LAYOUTS: &'static [AudioLayout] = &[
            AudioLayout::MONO_EFFECT,
            AudioLayout::STEREO_EFFECT,
            AudioLayout::STEREO_INSTRUMENT,
        ];
        type Params = NoParams;

        fn new() -> Self {
            Self {
                params: Arc::new(NoParams {}),
                layouts: Vec::new(),
                channels: 0,
            }
        }
        fn params(&self) -> Arc<NoParams> {
            Arc::clone(&self.params)
        }
        fn set_layout(&mut self, layout: AudioLayout) {
            self.layouts.push(layout);
        }
        fn prepare(&mut self, spec: &ProcessSpec) {
            self.channels = spec.num_channels;
        }
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
            let keyed = if context.sidechain.is_some() {
                10.0
            } else {
                0.0
            };
            for channel in buffer.iter_channels_mut() {
                channel.fill(self.channels as f32 + keyed);
            }
        }
    }

    #[test]
    fn a_layout_chosen_reaches_the_plugin_before_it_is_prepared() {
        assert_eq!(
            crate::offered_layouts::<Shaped>(),
            [
                Shaped::LAYOUT,
                AudioLayout::MONO_EFFECT,
                AudioLayout::STEREO_EFFECT
            ]
        );
        let mut harness = Harness::<Shaped>::new(48_000.0, 16);
        let input = AudioBuffer::<f32>::new(2, 4);
        let sidechain = AudioBuffer::<f32>::new(2, 4);
        let output = harness.process_with_sidechain(&input, &sidechain);
        assert_eq!(output.channel(1), &[12.0; 4]);

        let mut harness = harness.with_layout(AudioLayout::MONO_EFFECT);
        assert_eq!(harness.layout(), AudioLayout::MONO_EFFECT);
        assert_eq!(
            harness.plugin_mut().layouts,
            [Shaped::LAYOUT, AudioLayout::MONO_EFFECT]
        );
        // One channel, and no sidechain even when one is played.
        let output = harness.process_with_sidechain(&input, &sidechain);
        assert_eq!(output.num_channels(), 1);
        assert_eq!(output.channel(0), &[1.0; 4]);
    }

    #[test]
    #[should_panic(expected = "does not offer")]
    fn a_layout_the_wrappers_would_not_offer_is_refused() {
        let _ = Harness::<Shaped>::new(48_000.0, 16).with_layout(AudioLayout::STEREO_INSTRUMENT);
    }
}
