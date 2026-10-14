use std::sync::Arc;

use manguebyte_core::{AudioBuffer, NoDenormals};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Transport};
use manguebyte_params::{LoadReport, ParamSet};

use crate::editor::EditorSpec;
use crate::{ParamWatcher, Plugin, StateError, apply_param_events, load_state, save_state};

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
/// See the [crate documentation](crate) for an example.
pub struct Harness<P: Plugin> {
    plugin: P,
    params: Arc<P::Params>,
    /// Taken from the plugin once, as a wrapper does.
    editor: Option<Box<dyn EditorSpec>>,
    sample_rate: f64,
    max_block_size: usize,
    /// Block lengths to cycle through.
    pattern: Vec<usize>,
    next_in_pattern: usize,
    /// Events not yet delivered, sorted by frame.
    scheduled: Vec<(u64, EventKind)>,
    events: EventBuffer,
    /// The stretch of the sidechain that goes with the block being rendered.
    sidechain: AudioBuffer<f32>,
    watcher: ParamWatcher,
    transport: Transport,
    position: u64,
}

impl<P: Plugin> Harness<P> {
    /// Creates the plugin and prepares it.
    ///
    /// # Panics
    ///
    /// Panics if `max_block_size` is zero, or if the plugin is not well
    /// formed: its id is empty or contains spaces, it has no outputs, or its
    /// parameters fail [`ParamSet::validate`]. A plugin that cannot pass
    /// these checks would misbehave in a real host too.
    pub fn new(sample_rate: f64, max_block_size: usize) -> Self {
        assert!(max_block_size > 0, "max_block_size must be non-zero");
        let id = P::INFO.id;
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
        plugin.prepare(&ProcessSpec {
            sample_rate,
            max_block_size,
            num_channels: P::LAYOUT.outputs,
        });

        Self {
            plugin,
            sample_rate,
            max_block_size,
            pattern: vec![max_block_size],
            next_in_pattern: 0,
            scheduled: Vec::new(),
            events: EventBuffer::with_capacity(EVENTS_PER_BLOCK),
            sidechain: AudioBuffer::new(P::LAYOUT.sidechain, max_block_size),
            watcher: ParamWatcher::new(&*params),
            params,
            editor,
            transport: Transport::playing_at(120.0),
            position: 0,
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

    /// Sets the tempo the transport reports.
    #[must_use]
    pub fn with_tempo(mut self, bpm: f64) -> Self {
        self.transport.tempo_bpm = Some(bpm);
        self
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

    /// How many frames have been rendered so far.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Schedules an event at `frame` on the timeline.
    ///
    /// An event scheduled for a frame that has already been rendered is
    /// delivered at the start of the next block.
    pub fn schedule(&mut self, frame: u64, kind: EventKind) {
        let index = self.scheduled.partition_point(|&(at, _)| at <= frame);
        self.scheduled.insert(index, (frame, kind));
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
            .unwrap_or_else(|| panic!("plugin {} has no parameter {id:?}", P::INFO.id))
            .param_id();
        self.schedule(frame, EventKind::ParamValue { param, value });
    }

    /// Renders `frames` frames with silence at the input. This is how an
    /// instrument is played.
    pub fn render(&mut self, frames: usize) -> AudioBuffer<f32> {
        let mut output = AudioBuffer::new(P::LAYOUT.outputs, frames);
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
        let mut output = AudioBuffer::new(P::LAYOUT.outputs, input.num_frames());
        let shared = P::LAYOUT
            .inputs
            .min(P::LAYOUT.outputs)
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
            let mut context = ProcessContext::new(&self.events, &self.transport);
            if let Some(whole) = sidechain {
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
            buffer.windowed(done..done + length, |block| plugin.process(block, &context));

            self.transport.advance(length, self.sample_rate);
            self.position += length as u64;
            done += length;
        }
    }

    /// Moves the scheduled events that fall inside the next block into the
    /// event buffer.
    fn gather_events(&mut self, length: usize) {
        let end = self.position + length as u64;
        let due = self.scheduled.partition_point(|&(at, _)| at < end);
        for (at, kind) in self.scheduled.drain(..due) {
            let offset = at.saturating_sub(self.position) as u32;
            self.events
                .push(Event { offset, kind })
                .expect("more events in one block than the harness can deliver");
        }
    }
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
}
