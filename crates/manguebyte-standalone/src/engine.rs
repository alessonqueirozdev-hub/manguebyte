use std::sync::Arc;

use manguebyte_core::spsc::Consumer;
use manguebyte_core::{AudioBuffer, NoDenormals};
use manguebyte_devices::midi::{MidiReceiver, MidiTransmitter};
use manguebyte_dsp::ProcessContext;
use manguebyte_dsp::ProcessSpec;
use manguebyte_dsp::event::EventBuffer;
use manguebyte_dsp::note_input::NoteInput;
use manguebyte_dsp::timeline::{TempoMap, TimeSignature, TransportClock};
use manguebyte_midi::MidiMessage;
use manguebyte_params::ParamSet;
use manguebyte_plugin::output::{OutputBuffer, OutputProtocol, OutputTranslator};
use manguebyte_plugin::{AuxOutputs, ParamWatcher, Plugin};

/// Most events delivered to the plugin in one block.
const EVENTS_PER_BLOCK: usize = 2_048;

/// The part of a standalone application that runs on the audio thread.
///
/// It does for a plugin what a host does: gathers MIDI into events with
/// their frames, turns parameter changes into events, plays a transport
/// moving and maps the plugin's channels onto the device's.
/// [`Standalone`](crate::Standalone) owns one and gives it to the device
/// callback; it is public so that the same logic can be driven by something
/// else, such as a test or a custom audio back end.
pub struct Engine<P: Plugin> {
    plugin: P,
    params: Arc<P::Params>,
    watcher: ParamWatcher,
    events: EventBuffer,
    /// The transport, which the plugin also finds as a tempo map in
    /// [`ProcessContext::timeline`].
    clock: TransportClock,
    sample_rate: f64,
    midi: Vec<MidiReceiver>,
    notes: Consumer<MidiMessage>,
    /// Reads the MIDI of the ports and the program, as [`Plugin::MPE`]
    /// says.
    input: NoteInput,
    /// The block in the plugin's own channel layout.
    work: AudioBuffer<f32>,
    /// What the plugin sends out during a block, with room if it says it
    /// sends anything.
    output: OutputBuffer,
    /// Where what it sends goes, written for the port, if anywhere.
    midi_out: Option<(OutputTranslator, MidiTransmitter)>,
    /// The plugin's auxiliary output buses.
    aux: AuxOutputs,
    /// The device channel each auxiliary bus's first channel goes to, the
    /// others following it, or `None` for a bus that is not heard.
    aux_channels: Vec<Option<usize>>,
}

/// The device channels the auxiliary buses of `P` go to unless told
/// otherwise: those after the main output's (two for a mono plugin, which
/// is heard on both speakers), one bus after another, for the buses on by
/// default; a bus off by default is not heard.
fn default_aux_channels<P: Plugin>() -> Vec<Option<usize>> {
    let mut next = match P::LAYOUT.outputs {
        1 => 2,
        outputs => outputs,
    };
    P::AUX_OUTPUTS
        .iter()
        .map(|aux| {
            aux.active_by_default.then(|| {
                let first = next;
                next += aux.channels;
                first
            })
        })
        .collect()
}

impl<P: Plugin> Engine<P> {
    /// Creates the plugin and prepares it.
    ///
    /// `midi` are the ports to read; `notes` carries messages sent by the
    /// program itself. The transport plays from the start at `tempo_bpm` in
    /// 4/4 (120 bpm if `tempo_bpm` is not a positive, finite number);
    /// [`with_transport_clock`](Self::with_transport_clock) puts another
    /// clock in its place. Returns a description of the problem if the
    /// sample rate is not a positive, finite number, or if the plugin's
    /// parameters do not pass validation.
    pub fn new(
        sample_rate: f64,
        max_block_size: usize,
        tempo_bpm: f64,
        midi: Vec<MidiReceiver>,
        notes: Consumer<MidiMessage>,
    ) -> Result<Self, String> {
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            return Err(format!("{sample_rate} Hz is not a sample rate"));
        }
        let mut plugin = P::new();
        let params = plugin.params();
        params.validate().map_err(|error| error.to_string())?;
        let map = TempoMap::new(tempo_bpm, TimeSignature::FOUR_FOUR).unwrap_or_default();
        let mut clock = TransportClock::new(map, sample_rate);
        clock.play();
        plugin.prepare(&ProcessSpec {
            sample_rate,
            max_block_size,
            num_channels: P::LAYOUT.outputs,
        });
        Ok(Self {
            watcher: ParamWatcher::new(&*params),
            plugin,
            params,
            events: EventBuffer::with_capacity(EVENTS_PER_BLOCK),
            clock,
            sample_rate,
            midi,
            notes,
            input: NoteInput::new(P::MPE),
            work: AudioBuffer::new(P::LAYOUT.outputs, max_block_size),
            output: if P::NOTE_OUTPUT {
                OutputBuffer::with_capacity(EVENTS_PER_BLOCK)
            } else {
                OutputBuffer::new()
            },
            midi_out: None,
            aux: AuxOutputs::for_plugin::<P>(max_block_size),
            aux_channels: default_aux_channels::<P>(),
        })
    }

    /// Sends the plugin's auxiliary output buses, those of
    /// [`Plugin::AUX_OUTPUTS`], to other device channels: for each bus in
    /// order, the device channel its first channel goes to, its others
    /// following, or `None` to leave it unheard. Buses past the end of
    /// `channels` keep where they go by default, which is the device
    /// channels after the main output's, one bus after another, for every
    /// bus on by default. Allocates.
    ///
    /// A bus is mixed into what is already on its channels, so two buses,
    /// or a bus and the main output, can share them. A bus that is not
    /// heard, or whose first channel the device does not have, is not
    /// given to the plugin, which spends nothing on it; channels past the
    /// device's last are dropped.
    #[must_use]
    pub fn with_aux_channels(mut self, channels: &[Option<usize>]) -> Self {
        for (slot, &first) in self.aux_channels.iter_mut().zip(channels) {
            *slot = first;
        }
        self
    }

    /// The device channel each auxiliary bus's first channel goes to, or
    /// `None` for a bus that is not heard. See
    /// [`with_aux_channels`](Self::with_aux_channels).
    pub fn aux_channels(&self) -> &[Option<usize>] {
        &self.aux_channels
    }

    /// Sends what the plugin sends out, from
    /// [`Plugin::process_with_output`], to a MIDI output through
    /// `transmitter`, written in `protocol` by an [`OutputTranslator`].
    /// For a plugin that does not set [`Plugin::NOTE_OUTPUT`] nothing is
    /// ever sent. Allocates.
    ///
    /// Each packet goes on the frame of the event it came from, one past
    /// the block on its last, and the transmitter is told when each block
    /// begins, at the start of [`render`](Self::render), so that packets
    /// leave as far apart as their frames are.
    #[must_use]
    pub fn with_midi_output(
        mut self,
        transmitter: MidiTransmitter,
        protocol: OutputProtocol,
    ) -> Self {
        self.midi_out = Some((OutputTranslator::new(protocol), transmitter));
        self
    }

    /// Plays `clock` in place of the engine's own transport, for example
    /// one made by [`TransportClock::with_control`], whose
    /// [`ClockControl`](manguebyte_dsp::timeline::ClockControl) then works
    /// the transport from another thread. The clock's sample rate is set
    /// to the engine's. Frees the engine's own clock, so call it before the
    /// engine goes to the audio thread.
    #[must_use]
    pub fn with_transport_clock(mut self, mut clock: TransportClock) -> Self {
        clock.set_sample_rate(self.sample_rate);
        self.clock = clock;
        self
    }

    /// The transport the plugin is played with.
    pub fn transport_clock(&self) -> &TransportClock {
        &self.clock
    }

    /// The plugin's parameters.
    pub fn params(&self) -> Arc<P::Params> {
        Arc::clone(&self.params)
    }

    /// Produces one block for the device.
    ///
    /// `input` is what the device captured, if anything. `output` is the
    /// device's buffer, in the device's channel count; it must not be longer
    /// than the block size the engine was created with. Real-time safe.
    pub fn render(&mut self, input: Option<&AudioBuffer<f32>>, output: &mut AudioBuffer<f32>) {
        let _quiet = NoDenormals::new();
        let frames = output.num_frames().min(self.work.capacity());
        if let Some((_, transmitter)) = &mut self.midi_out {
            transmitter.begin_block(frames, self.sample_rate);
        }
        let params: &dyn ParamSet = &*self.params;

        // Parameter changes first, then notes from the program, then MIDI
        // ports, each message on the frame its arrival time maps to.
        self.events.clear();
        self.watcher.emit_changes(params, &mut self.events);
        let announced = self.events.len();
        // All of it is read through one `NoteInput`, as one stream, so that
        // MPE and the notes it holds are followed as the plugin asked.
        let Self {
            events,
            input: reader,
            notes,
            midi,
            sample_rate,
            ..
        } = self;
        while let Some(message) = notes.pop() {
            reader.midi1(0, message, |event| {
                let _ = events.push(event);
            });
        }
        for receiver in midi {
            receiver.read_events(frames, *sample_rate, reader, events);
        }
        // Only what came after the announcements is stored: the announced
        // values were read from the parameters, and storing them again
        // would undo an edit the interface made in the meantime.
        self.watcher
            .apply_and_remember(params, &self.events.as_slice()[announced..]);

        self.work.set_num_frames(frames);
        self.work.clear();
        if let Some(input) = input {
            let channels = P::LAYOUT
                .inputs
                .min(self.work.num_channels())
                .min(input.num_channels());
            let shared = frames.min(input.num_frames());
            for channel in 0..channels {
                self.work.channel_mut(channel)[..shared]
                    .copy_from_slice(&input.channel(channel)[..shared]);
            }
        }

        let block = self.clock.next_block(frames);
        let map = self.clock.map();
        let transport = block.transport(map);
        let context =
            ProcessContext::new(&self.events, &transport).with_timeline(block.with_map(map));
        self.output.clear();
        // A bus is given to the plugin only if the device has somewhere
        // for it to go.
        let device_channels = output.num_channels();
        for (bus, first) in self.aux_channels.iter().enumerate() {
            let heard = first.is_some_and(|first| first < device_channels);
            self.aux.set_active(bus, heard);
        }
        self.aux.start_block(frames);
        self.plugin
            .process_with_aux(&mut self.work, &context, &mut self.output, &mut self.aux);
        if let Some((translator, transmitter)) = &mut self.midi_out {
            let last = frames.saturating_sub(1) as u32;
            // What does not fit in the queue is dropped, and counted by
            // the transmitter.
            translator.translate_buffer(&self.output, |offset, packet| {
                transmitter.send(offset.min(last), packet);
            });
        }

        // A mono plugin is heard on both speakers; otherwise channels map
        // one to one and any extra device channels stay silent.
        let plugin_channels = self.work.num_channels();
        for channel in 0..output.num_channels() {
            let source = if plugin_channels == 1 && channel < 2 {
                Some(0)
            } else {
                (channel < plugin_channels).then_some(channel)
            };
            let target = output.channel_mut(channel);
            match source {
                Some(source) => {
                    target[..frames].copy_from_slice(self.work.channel(source));
                    target[frames..].fill(0.0);
                }
                None => target.fill(0.0),
            }
        }
        for (bus, &first) in self.aux_channels.iter().enumerate() {
            let (Some(first), Some(written)) = (first, self.aux.bus(bus)) else {
                continue;
            };
            for (channel, samples) in written.iter_channels().enumerate() {
                if first + channel >= device_channels {
                    break;
                }
                let target = &mut output.channel_mut(first + channel)[..frames];
                for (out, &sample) in target.iter_mut().zip(samples) {
                    *out += sample;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manguebyte_core::spsc::{self, Producer};
    use manguebyte_dsp::event::{EventKind, NoteRef};
    use manguebyte_dsp::{Processor, Synth, Voice};
    use manguebyte_params::{FloatParam, FloatRange, param_set};
    use manguebyte_plugin::{AudioLayout, PluginInfo, PluginKind};

    struct LevelParams {
        level: FloatParam,
    }
    param_set!(LevelParams { level });

    /// Outputs the key number of each held note, times `level`.
    #[derive(Default)]
    struct KeyVoice(f32);
    impl Voice for KeyVoice {
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn note_on(&mut self, note: NoteRef, _: f32) {
            self.0 = f32::from(note.key);
        }
        fn note_off(&mut self, _: f32) {
            self.0 = 0.0;
        }
        fn choke(&mut self) {
            self.0 = 0.0;
        }
        fn is_active(&self) -> bool {
            self.0 > 0.0
        }
        fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
            for channel in buffer.iter_channels_mut() {
                channel.iter_mut().for_each(|sample| *sample += self.0);
            }
        }
    }

    struct Keys {
        synth: Synth<KeyVoice>,
        params: Arc<LevelParams>,
        level: f32,
    }

    impl Plugin for Keys {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.standalone-keys",
            name: "Keys",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Plays key numbers",
            kind: PluginKind::Instrument,
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
        type Params = LevelParams;

        fn new() -> Self {
            Self {
                synth: Synth::with_voices(4, KeyVoice::default),
                params: Arc::new(LevelParams {
                    level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 2.0), 1.0),
                }),
                level: 1.0,
            }
        }
        fn params(&self) -> Arc<LevelParams> {
            Arc::clone(&self.params)
        }
        fn prepare(&mut self, spec: &ProcessSpec) {
            self.synth.prepare(spec);
        }
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
            // Follows events only, like a plugin that never polls.
            for event in context.events {
                if let EventKind::ParamValue { value, .. } = event.kind {
                    self.level = value as f32;
                }
            }
            self.synth.process_with(buffer, context);
            buffer.apply_gain(self.level);
        }
    }

    /// Scales its mono input.
    struct Half(Arc<LevelParams>);
    impl Plugin for Half {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.standalone-half",
            name: "Half",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Halves the input",
            kind: PluginKind::Effect,
        };
        const LAYOUT: AudioLayout = AudioLayout::MONO_EFFECT;
        type Params = LevelParams;

        fn new() -> Self {
            Self(Arc::new(LevelParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 2.0), 0.5),
            }))
        }
        fn params(&self) -> Arc<LevelParams> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
            buffer.apply_gain(self.0.level.value());
        }
    }

    /// One level, and an edit that lands the moment the level has been read:
    /// the interface storing a value while a block is being prepared.
    struct Meddled {
        level: FloatParam,
        edit: std::sync::Mutex<Option<f32>>,
    }
    impl ParamSet for Meddled {
        fn visit<'a>(&'a self, visitor: &mut dyn FnMut(&'a dyn manguebyte_params::Param)) {
            visitor(&self.level);
            if let Some(value) = self.edit.lock().unwrap().take() {
                self.level.set(value);
            }
        }
    }

    struct Edited(Arc<Meddled>);
    impl Plugin for Edited {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.standalone-edited",
            name: "Edited",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Scales its input by a level edited mid-block",
            kind: PluginKind::Effect,
        };
        const LAYOUT: AudioLayout = AudioLayout::MONO_EFFECT;
        type Params = Meddled;

        fn new() -> Self {
            Self(Arc::new(Meddled {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 2.0), 0.5),
                edit: std::sync::Mutex::new(None),
            }))
        }
        fn params(&self) -> Arc<Meddled> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
            buffer.apply_gain(self.0.level.value());
        }
    }

    #[test]
    fn an_edit_made_while_a_block_is_prepared_is_kept() {
        let (mut engine, _notes) = engine::<Edited>();
        let params = engine.params();
        let mut device = AudioBuffer::<f32>::new(1, 8);
        engine.render(None, &mut device);

        // The block announces 1.0, and 1.5 is stored right after it is read.
        params.level.set(1.0);
        *params.edit.lock().unwrap() = Some(1.5);
        engine.render(None, &mut device);
        assert_eq!(params.level.value(), 1.5, "the edit was undone");

        // Nothing undoes it later either.
        engine.render(None, &mut device);
        assert_eq!(params.level.value(), 1.5);
    }

    /// Sends every note it is given back out a fifth higher, three frames
    /// later, bent up a semitone.
    struct Fifths(Arc<LevelParams>);
    impl Plugin for Fifths {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.standalone-fifths",
            name: "Fifths",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Sends notes out a fifth up",
            kind: PluginKind::NoteEffect,
        };
        const LAYOUT: AudioLayout = AudioLayout::MONO_EFFECT;
        const NOTE_OUTPUT: bool = true;
        type Params = LevelParams;

        fn new() -> Self {
            Self(Arc::new(LevelParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 2.0), 0.5),
            }))
        }
        fn params(&self) -> Arc<LevelParams> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
            self.process_with_output(buffer, context, &mut OutputBuffer::new());
        }
        fn process_with_output(
            &mut self,
            _: &mut AudioBuffer<f32>,
            context: &ProcessContext<'_, f32>,
            output: &mut OutputBuffer,
        ) {
            use manguebyte_dsp::event::Expression;
            let up = |note: NoteRef| NoteRef::new(note.channel, note.key + 7);
            for event in context.events {
                match event.kind {
                    EventKind::NoteOn { note, velocity } => {
                        output.note_on(event.offset + 3, up(note), velocity);
                        output.expression(event.offset + 3, up(note), Expression::Tuning, 1.0);
                    }
                    EventKind::NoteOff { note, velocity } => {
                        output.note_off(event.offset, up(note), velocity);
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn notes_the_plugin_sends_reach_the_midi_output() {
        use manguebyte_devices::midi::midi_output_queue;
        use manguebyte_midi::mpe::Zone;
        use manguebyte_midi::ump::Packet;

        let cc = |channel, controller, value| MidiMessage::ControlChange {
            channel,
            controller,
            value,
        };
        let note_on = |channel| MidiMessage::NoteOn {
            channel,
            note: 67,
            velocity: 100,
        };
        let note_off = |channel| MidiMessage::NoteOff {
            channel,
            note: 67,
            velocity: 0,
        };
        let plain = vec![note_on(0), note_off(0)];
        let mpe = vec![
            cc(0, 100, 6),
            cc(0, 101, 0),
            cc(0, 6, 15),
            MidiMessage::PitchBend {
                channel: 1,
                value: 0,
            },
            MidiMessage::ChannelPressure {
                channel: 1,
                pressure: 0,
            },
            cc(1, 74, 64),
            note_on(1),
            // A semitone of the member channels' 48, in 14 bits.
            MidiMessage::PitchBend {
                channel: 1,
                value: 170,
            },
            note_off(1),
        ];
        for (protocol, expected) in [
            (OutputProtocol::Midi1, plain),
            (OutputProtocol::Mpe(Zone::lower(15).unwrap()), mpe),
        ] {
            let (transmitter, mut queue) = midi_output_queue(64);
            let (engine, mut notes) = engine::<Fifths>();
            let mut engine = engine.with_midi_output(transmitter, protocol);
            let mut device = AudioBuffer::<f32>::new(1, 8);
            notes.push(on(60)).unwrap();
            engine.render(None, &mut device);
            notes
                .push(MidiMessage::NoteOff {
                    channel: 0,
                    note: 60,
                    velocity: 0,
                })
                .unwrap();
            engine.render(None, &mut device);
            let mut sent: Vec<Packet> = Vec::new();
            queue.send_all(|packet| sent.push(*packet));
            let messages: Vec<MidiMessage> = sent
                .iter()
                .map(|packet| packet.to_midi1().unwrap())
                .collect();
            assert_eq!(messages, expected, "{protocol:?}");
        }

        // A plugin that sends nothing sends nothing, output or not.
        let (transmitter, mut queue) = midi_output_queue(64);
        let (engine, mut notes) = engine::<Keys>();
        let mut engine = engine.with_midi_output(transmitter, OutputProtocol::Midi2);
        notes.push(on(60)).unwrap();
        engine.render(None, &mut AudioBuffer::<f32>::new(2, 8));
        let mut count = 0;
        queue.send_all(|_| count += 1);
        assert_eq!(count, 0);
    }

    fn engine<P: Plugin>() -> (Engine<P>, Producer<MidiMessage>) {
        let (notes, receiver) = spsc::channel(64);
        let engine = Engine::<P>::new(48_000.0, 16, 120.0, Vec::new(), receiver).unwrap();
        (engine, notes)
    }

    fn on(note: u8) -> MidiMessage {
        MidiMessage::NoteOn {
            channel: 0,
            note,
            velocity: 100,
        }
    }

    #[test]
    fn notes_sent_by_the_program_are_played_and_parameters_are_followed() {
        let (mut engine, mut notes) = engine::<Keys>();
        let params = engine.params();
        let mut device = AudioBuffer::<f32>::new(2, 8);

        engine.render(None, &mut device);
        assert_eq!(device.peak(), 0.0);

        notes.push(on(60)).unwrap();
        notes.push(on(64)).unwrap();
        engine.render(None, &mut device);
        assert_eq!(device.channel(0), &[124.0; 8]);
        assert_eq!(device.channel(1), &[124.0; 8]);

        // A parameter written from another thread reaches a plugin that
        // only follows events.
        params.level.set(0.5);
        notes
            .push(MidiMessage::NoteOff {
                channel: 0,
                note: 60,
                velocity: 0,
            })
            .unwrap();
        engine.render(None, &mut device);
        assert_eq!(device.channel(0), &[32.0; 8]);
    }

    #[test]
    fn mpe_sent_by_the_program_is_read_as_per_note_events() {
        use manguebyte_midi::mpe::{Configuration, ZoneKind};

        let (mut engine, mut notes) = engine::<Keys>();
        let mut device = AudioBuffer::<f32>::new(2, 8);
        let lower = Configuration {
            zone: ZoneKind::Lower,
            members: 15,
        };
        for &message in lower.messages().as_slice() {
            notes.push(message).unwrap();
        }
        // The same key on two member channels: two notes, which end one
        // at a time.
        for channel in [1, 2] {
            notes
                .push(MidiMessage::NoteOn {
                    channel,
                    note: 30,
                    velocity: 100,
                })
                .unwrap();
        }
        engine.render(None, &mut device);
        assert_eq!(device.channel(0), &[60.0; 8]);
        notes
            .push(MidiMessage::NoteOff {
                channel: 2,
                note: 30,
                velocity: 0,
            })
            .unwrap();
        engine.render(None, &mut device);
        assert_eq!(device.channel(0), &[30.0; 8]);
        notes
            .push(MidiMessage::NoteOff {
                channel: 1,
                note: 30,
                velocity: 0,
            })
            .unwrap();
        engine.render(None, &mut device);
        assert_eq!(device.peak(), 0.0);
    }

    #[test]
    fn the_plugin_layout_is_mapped_onto_the_device() {
        // A stereo plugin on a device with four outputs: the extra two
        // stay silent.
        let (mut engine, mut notes) = engine::<Keys>();
        notes.push(on(10)).unwrap();
        let mut quad = AudioBuffer::<f32>::new(4, 8);
        quad.channel_mut(3).fill(9.0);
        engine.render(None, &mut quad);
        assert_eq!(quad.channel(1), &[10.0; 8]);
        assert_eq!(quad.channel(2), &[0.0; 8]);
        assert_eq!(quad.channel(3), &[0.0; 8]);

        // On a mono device, the left channel is what is heard.
        let mut mono = AudioBuffer::<f32>::new(1, 8);
        engine.render(None, &mut mono);
        assert_eq!(mono.channel(0), &[10.0; 8]);
    }

    #[test]
    fn an_effect_processes_the_captured_input_and_mono_goes_to_both_speakers() {
        let (mut engine, _notes) = engine::<Half>();
        let mut captured = AudioBuffer::<f32>::new(2, 8);
        captured.channel_mut(0).fill(1.0);
        captured.channel_mut(1).fill(100.0); // not an input of a mono plugin
        let mut device = AudioBuffer::<f32>::new(2, 8);
        engine.render(Some(&captured), &mut device);
        assert_eq!(device.channel(0), &[0.5; 8]);
        assert_eq!(device.channel(1), &[0.5; 8]);

        // With no input device, an effect hears silence.
        engine.render(None, &mut device);
        assert_eq!(device.peak(), 0.0);
    }

    #[test]
    fn a_block_longer_than_expected_does_not_panic() {
        let (mut engine, mut notes) = engine::<Keys>();
        notes.push(on(10)).unwrap();
        let mut device = AudioBuffer::<f32>::new(2, 40);
        device.channel_mut(0).fill(7.0);
        engine.render(None, &mut device);
        // What fits is rendered; the rest is silence rather than garbage.
        assert_eq!(&device.channel(0)[..16], &[10.0; 16]);
        assert_eq!(&device.channel(0)[16..], &[0.0; 24]);
    }

    /// Writes the beat of every frame on the left, and the tempo on the
    /// right, from the timeline in its context.
    struct Beats(Arc<LevelParams>);
    impl Plugin for Beats {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.standalone-beats",
            name: "Beats",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Writes where the music is",
            kind: PluginKind::Instrument,
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
        type Params = LevelParams;

        fn new() -> Self {
            Self(Arc::new(LevelParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 2.0), 1.0),
            }))
        }
        fn params(&self) -> Arc<LevelParams> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
            let timeline = context.timeline.expect("the engine gives a timeline");
            assert_eq!(*context.transport, timeline.transport());
            for frame in 0..buffer.num_frames() {
                buffer.channel_mut(0)[frame] = timeline.beat_at(frame) as f32;
                buffer.channel_mut(1)[frame] = timeline.tempo_at(frame) as f32;
            }
        }
    }

    #[test]
    fn the_plugin_plays_with_a_transport_the_program_works() {
        use manguebyte_dsp::timeline::{TempoCurve, TempoMap, TimeSignature, TransportClock};

        let (engine, _notes) = engine::<Beats>();
        // At the tempo it was made with, from the start.
        assert!(engine.transport_clock().is_playing());
        assert_eq!(engine.transport_clock().map().tempo_at_beat(0.0), 120.0);

        // A clock worked from elsewhere, made at another rate: the engine
        // puts it at its own.
        let (clock, control) = TransportClock::with_control(TempoMap::default(), 96_000.0);
        let mut engine = engine.with_transport_clock(clock);
        assert_eq!(engine.transport_clock().sample_rate(), 48_000.0);
        let mut device = AudioBuffer::<f32>::new(2, 16);

        // Stopped until told to play.
        engine.render(None, &mut device);
        assert_eq!(device.channel(0), &[0.0; 16]);
        assert!(control.play());
        assert!(control.seek_beat(2.0));
        assert!(control.set_loop_frames(Some((48_000, 48_008))));
        engine.render(None, &mut device);
        // Frame 48 000 is beat 2; the loop goes round after 8 frames.
        let step = 2.0 / 48_000.0;
        for frame in 0..16 {
            let expected = 2.0 + (frame % 8) as f64 * step;
            assert_eq!(device.channel(0)[frame], expected as f32);
        }
        assert_eq!(control.position_frames(), 48_000);
        assert_eq!(control.position_beats(), 2.0);

        // A tempo map handed over while it plays: a ramp from 120 to 240.
        let mut map = TempoMap::new(120.0, TimeSignature::FOUR_FOUR).unwrap();
        map.set_tempo(2.0, 120.0, TempoCurve::Linear).unwrap();
        map.set_tempo(3.0, 240.0, TempoCurve::Step).unwrap();
        control.set_tempo_map(map).unwrap();
        assert!(control.set_loop_frames(None));
        engine.render(None, &mut device);
        assert_eq!(device.channel(1)[0], 120.0);
        assert!(device.channel(1)[15] > 120.0);
        assert_eq!(
            control.tempo_bpm(),
            engine
                .transport_clock()
                .map()
                .tempo_at_frame(48_016, 48_000.0)
        );
        // The map it replaced came back to be freed off the audio thread.
        assert_eq!(control.collect(), 1);
        assert!(control.stop());
        engine.render(None, &mut device);
        assert!(!control.is_playing());
    }

    /// How many blocks each of `Divisions`' buses was given in.
    static GIVEN: [std::sync::atomic::AtomicUsize; 3] =
        [const { std::sync::atomic::AtomicUsize::new(0) }; 3];

    /// 0.5 on its main output, and bus `b` channel `c` at `(b + 1) + c / 10`.
    struct Divisions(Arc<LevelParams>);
    impl Plugin for Divisions {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.standalone-divisions",
            name: "Divisions",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Each division on a bus of its own",
            kind: PluginKind::Instrument,
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
        const AUX_OUTPUTS: &'static [manguebyte_plugin::AuxOutput] = &[
            manguebyte_plugin::AuxOutput::stereo("Great"),
            manguebyte_plugin::AuxOutput::new("Pedal", 1).inactive(),
            manguebyte_plugin::AuxOutput::stereo("Swell"),
        ];
        type Params = LevelParams;

        fn new() -> Self {
            Self(Arc::new(LevelParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 1.0), 0.5),
            }))
        }
        fn params(&self) -> Arc<LevelParams> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
        fn process_with_aux(
            &mut self,
            buffer: &mut AudioBuffer<f32>,
            _: &ProcessContext<'_, f32>,
            _: &mut OutputBuffer,
            aux: &mut AuxOutputs,
        ) {
            for channel in buffer.iter_channels_mut() {
                channel.fill(0.5);
            }
            for (bus, given) in GIVEN.iter().enumerate() {
                if let Some(written) = aux.bus_mut(bus) {
                    given.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    for (channel, samples) in written.iter_channels_mut().enumerate() {
                        samples.fill((bus + 1) as f32 + channel as f32 / 10.0);
                    }
                }
            }
        }
    }

    fn device_channels(device: &AudioBuffer<f32>) -> Vec<f32> {
        device.iter_channels().map(|channel| channel[3]).collect()
    }

    #[test]
    fn each_bus_goes_to_device_channels_of_its_own() {
        use std::sync::atomic::Ordering::Relaxed;

        // By default: after the main output, the buses on by default, one
        // after another. The pedal is off by default and not given.
        let (mut engine, _notes) = engine::<Divisions>();
        assert_eq!(engine.aux_channels(), [Some(2), None, Some(4)]);
        let mut device = AudioBuffer::<f32>::new(7, 16);
        engine.render(None, &mut device);
        assert_eq!(
            device_channels(&device),
            [0.5, 0.5, 1.0, 1.1, 3.0, 3.1, 0.0]
        );
        assert_eq!(GIVEN[1].load(Relaxed), 0, "the pedal is not rendered");

        // Routed by hand: the pedal heard on the last channel, the swell
        // mixed into the main output, and the great on channels the device
        // lacks, so that it is not rendered either.
        let great = GIVEN[0].load(Relaxed);
        let mut engine = engine.with_aux_channels(&[Some(9), Some(6), Some(0)]);
        engine.render(None, &mut device);
        assert_eq!(
            device_channels(&device),
            [3.5, 3.6, 0.0, 0.0, 0.0, 0.0, 2.0]
        );
        assert_eq!(GIVEN[0].load(Relaxed), great, "nowhere to go, not rendered");
        assert!(GIVEN[1].load(Relaxed) > 0);

        // A device with fewer channels hears what fits.
        let mut stereo = AudioBuffer::<f32>::new(2, 16);
        let mut engine = engine.with_aux_channels(&[Some(1), None, None]);
        engine.render(None, &mut stereo);
        assert_eq!(device_channels(&stereo), [0.5, 1.5]);
    }
}
