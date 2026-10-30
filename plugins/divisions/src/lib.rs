//! Mangue Divisions: an example instrument with several output buses.
//!
//! A small organ of four divisions, each played from a MIDI channel of
//! its own: channel 1 the Great, 2 the Swell, 3 the Choir, 4 the Pedal;
//! notes on any other channel play the Great. Each division has its own
//! voices and its own timbre, a few sine partials, and goes out on an
//! auxiliary output bus of its own ([`Plugin::AUX_OUTPUTS`]), so that a
//! host can mix and place the divisions apart; the main output carries
//! them all together, for a host that reads only it.
//!
//! It shows what a plugin with several outputs does:
//!
//! - it declares its buses, each with a name and a width;
//! - it writes them from [`Plugin::process_with_aux`], where a bus the
//!   host has switched off is not given, and the division renders only
//!   into the main output;
//! - it is exported as a CLAP, a VST 3, an LV2 plugin and an Audio Unit,
//!   whose wrappers present the buses as ports, buses, port groups and
//!   output elements.
//!
//! Run `scripts/bundle-clap.sh mangue-divisions "Mangue Divisions"` for a
//! `.clap` file and `scripts/bundle-vst3.sh mangue-divisions "Mangue
//! Divisions"` for a `.vst3` bundle; `cargo mangue bundle -p
//! mangue-divisions --format lv2` writes its `.lv2` bundle.

use std::sync::Arc;

use manguebyte::dsp::event::{EventBuffer, EventKind, NoteRef};
use manguebyte::plugin::output::OutputBuffer;
use manguebyte::prelude::*;

/// Most events a division is handed in one block.
const EVENTS_PER_DIVISION: usize = 1_024;

/// The divisions, in the order of their buses and their MIDI channels.
pub const DIVISIONS: [AuxOutput; 4] = [
    AuxOutput::stereo("Great"),
    AuxOutput::stereo("Swell"),
    AuxOutput::stereo("Choir"),
    AuxOutput::stereo("Pedal"),
];

/// The partials of each division's pipes: (multiple of the note's
/// frequency, level).
const PARTIALS: [[(f64, f32); 3]; 4] = [
    // Great: a principal, with its octave and twelfth.
    [(1.0, 0.6), (2.0, 0.3), (3.0, 0.1)],
    // Swell: softer and reedier.
    [(1.0, 0.5), (3.0, 0.25), (5.0, 0.12)],
    // Choir: a flute.
    [(1.0, 0.7), (2.0, 0.08), (4.0, 0.02)],
    // Pedal: an octave below what is played.
    [(0.5, 0.7), (1.0, 0.25), (1.5, 0.05)],
];

/// The division a note on MIDI `channel` (from 0) plays.
pub fn division_of(channel: u8) -> usize {
    match channel {
        0..=3 => usize::from(channel),
        _ => 0,
    }
}

/// The plugin's parameters.
pub struct DivisionsParams {
    /// Output level in decibels, for every division alike.
    pub volume: FloatParam,
}

param_set!(DivisionsParams { volume });

/// One pipe sounding: a few sine partials with a short attack and release.
pub struct Pipe {
    partials: [(f64, f32); 3],
    sample_rate: f64,
    /// Each partial's phase, in cycles.
    phases: [f64; 3],
    frequency: f64,
    level: f32,
    /// Where the envelope is, from 0 to 1, and how far it moves each frame.
    envelope: f32,
    step: f32,
    releasing: bool,
    active: bool,
}

impl Pipe {
    fn new(division: usize) -> Self {
        Self {
            partials: PARTIALS[division],
            sample_rate: 48_000.0,
            phases: [0.0; 3],
            frequency: 440.0,
            level: 0.0,
            envelope: 0.0,
            step: 0.0,
            releasing: false,
            active: false,
        }
    }

    /// The envelope's step per frame to cover the whole range in `seconds`.
    fn step_for(&self, seconds: f64) -> f32 {
        (1.0 / (seconds * self.sample_rate)) as f32
    }
}

impl Voice for Pipe {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        self.frequency = 440.0 * 2f64.powf((f64::from(note.key) - 69.0) / 12.0);
        // An organ plays its pipes at one level, whatever the touch; a
        // whisper of the velocity is kept so that the example responds.
        self.level = 0.15 * (0.75 + 0.25 * velocity);
        self.phases = [0.0; 3];
        self.envelope = 0.0;
        self.step = self.step_for(0.01);
        self.releasing = false;
        self.active = true;
    }

    fn note_off(&mut self, _velocity: f32) {
        self.releasing = true;
        self.step = self.step_for(0.12);
    }

    fn choke(&mut self) {
        self.active = false;
        self.envelope = 0.0;
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        let frames = buffer.num_frames();
        for frame in 0..frames {
            if self.releasing {
                self.envelope -= self.step;
                if self.envelope <= 0.0 {
                    self.choke();
                    return;
                }
            } else {
                self.envelope = (self.envelope + self.step).min(1.0);
            }
            let mut sample = 0.0f32;
            for ((multiple, level), phase) in self.partials.iter().zip(&mut self.phases) {
                sample += level * (*phase * std::f64::consts::TAU).sin() as f32;
                *phase = (*phase + multiple * self.frequency / self.sample_rate).fract();
            }
            let sample = sample * self.level * self.envelope;
            for channel in buffer.iter_channels_mut() {
                channel[frame] += sample;
            }
        }
    }
}

/// The plugin.
pub struct Divisions {
    params: Arc<DivisionsParams>,
    /// One set of voices per division.
    synths: [Synth<Pipe>; 4],
    /// The events of each division in the block being processed.
    events: [EventBuffer; 4],
    /// Where one division renders before it is mixed.
    scratch: AudioBuffer<f32>,
}

impl Plugin for Divisions {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.divisions",
        name: "Mangue Divisions",
        vendor: "Manguebyte",
        version: env!("CARGO_PKG_VERSION"),
        description: "A small organ with each division on a bus of its own",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    const AUX_OUTPUTS: &'static [AuxOutput] = &DIVISIONS;
    type Params = DivisionsParams;

    fn new() -> Self {
        let volume = FloatParam::new("volume", "Volume", FloatRange::linear(-48.0, 6.0), 0.0)
            .with_unit("dB");
        Self {
            params: Arc::new(DivisionsParams { volume }),
            synths: std::array::from_fn(|division| {
                Synth::with_voices(16, move || Pipe::new(division))
            }),
            events: std::array::from_fn(|_| EventBuffer::with_capacity(EVENTS_PER_DIVISION)),
            scratch: AudioBuffer::new(2, 0),
        }
    }

    fn params(&self) -> Arc<DivisionsParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, spec: &ProcessSpec) {
        for synth in &mut self.synths {
            synth.prepare(spec);
        }
        self.scratch = AudioBuffer::new(2, spec.max_block_size);
    }

    fn reset(&mut self) {
        for synth in &mut self.synths {
            synth.reset();
        }
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.process_with_aux(
            buffer,
            context,
            &mut OutputBuffer::new(),
            &mut AuxOutputs::new(),
        );
    }

    fn process_with_aux(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        context: &ProcessContext<'_, f32>,
        _output: &mut OutputBuffer,
        aux: &mut AuxOutputs,
    ) {
        // Notes go to their channel's division; everything else, such as
        // the sustain pedal and "all notes off", to every division.
        for events in &mut self.events {
            events.clear();
        }
        for event in context.events {
            let division = match event.kind {
                EventKind::NoteOn { note, .. } | EventKind::NoteOff { note, .. } => {
                    Some(division_of(note.channel))
                }
                _ => None,
            };
            for (index, events) in self.events.iter_mut().enumerate() {
                if division.is_none_or(|division| division == index) {
                    // A block with more events than this drops the rest.
                    let _ = events.push(*event);
                }
            }
        }

        let frames = buffer.num_frames();
        let gain = decibels::db_to_gain(f64::from(self.params.volume.value())) as f32;
        buffer.clear();
        self.scratch.set_num_frames(frames);
        for (division, synth) in self.synths.iter_mut().enumerate() {
            let piece = ProcessContext::new(&self.events[division], context.transport);
            synth.process_with(&mut self.scratch, &piece);
            self.scratch.apply_gain(gain);
            buffer.mix_from(&self.scratch, 1.0);
            // The division's own bus, if the host listens to it.
            if let Some(bus) = aux.bus_mut(division) {
                bus.copy_from(&self.scratch);
            }
        }
    }
}

manguebyte::clap::export_clap!(Divisions);
manguebyte::vst3::export_vst3!(Divisions);
manguebyte::lv2::export_lv2!(Divisions);
manguebyte_au::export_au!(Divisions);
