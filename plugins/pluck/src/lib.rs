//! Mangue Pluck: an example instrument built with Manguebyte.
//!
//! A polyphonic plucked synth: each note is a sawtooth through a resonant
//! low-pass filter that closes as the note fades, and the whole instrument
//! goes through a reverb. It is small on purpose. Its job is to show a
//! complete plugin in one file:
//!
//! - parameters with ranges, units and smoothing;
//! - a [`Voice`] driven by a [`Synth`], with notes on their exact frame;
//! - per-note expression (tuning, volume, pressure) and per-note modulation
//!   of the filter;
//! - sample-accurate automation;
//! - one line each to export it as a CLAP plugin and as a VST 3 plugin,
//!   from the same library;
//! - the smallest editor: a title and a knob for the output level (see
//!   [`Pluck::editor_spec`]). Mangue Duck has the fuller example.
//!
//! Run `scripts/bundle-clap.sh mangue-pluck "Mangue Pluck"` to get a `.clap`
//! file a host can load, and `scripts/bundle-vst3.sh mangue-pluck
//! "Mangue Pluck"` to get a `.vst3` bundle.

use std::sync::Arc;

use manguebyte::plugin::editor::{EditorSpec, HostSize};
use manguebyte::prelude::*;
use manguebyte_gui::Editor;
use manguebyte_ui::{IntoView, ParamHandle, column, knob, label};

/// The editor's size at scale 1, in logical pixels.
pub const EDITOR_SIZE: HostSize = HostSize::new(180, 150);

/// The plugin's parameters.
pub struct PluckParams {
    /// Filter cutoff at the top of each note, in hertz.
    pub cutoff: FloatParam,
    /// Filter resonance.
    pub resonance: FloatParam,
    /// How long a note takes to fade once the key is held, in seconds.
    pub decay: FloatParam,
    /// How long a note takes to fade after the key is released, in seconds.
    pub release: FloatParam,
    /// How much reverb is mixed in.
    pub reverb: FloatParam,
    /// Output level in decibels.
    pub level: FloatParam,
}
param_set!(PluckParams {
    cutoff,
    resonance,
    decay,
    release,
    reverb,
    level
});

const CUTOFF: ParamId = ParamId::from_name("cutoff");
const RESONANCE: ParamId = ParamId::from_name("resonance");
const DECAY: ParamId = ParamId::from_name("decay");
const RELEASE: ParamId = ParamId::from_name("release");

impl PluckParams {
    fn new() -> Self {
        let time = || FloatRange::skewed_around(0.01, 5.0, 0.5);
        Self {
            cutoff: FloatParam::new(
                "cutoff",
                "Cutoff",
                FloatRange::logarithmic(100.0, 12_000.0),
                1_800.0,
            )
            .with_unit("Hz")
            .with_precision(0),
            resonance: FloatParam::new("resonance", "Resonance", FloatRange::linear(0.5, 8.0), 2.0),
            decay: FloatParam::new("decay", "Decay", time(), 0.3).with_unit("s"),
            release: FloatParam::new("release", "Release", time(), 0.4).with_unit("s"),
            reverb: FloatParam::new("reverb", "Reverb", FloatRange::linear(0.0, 1.0), 0.25),
            level: FloatParam::new("level", "Level", FloatRange::linear(-40.0, 6.0), -12.0)
                .with_unit("dB")
                .with_precision(1),
        }
    }
}

/// One note.
struct PluckVoice {
    oscillator: Oscillator,
    filter: StateVariableFilter<f32>,
    envelope: Adsr,
    shape: AdsrParams,
    key: f64,
    velocity: f32,
    cutoff: f64,
    /// Per-note offsets, reset for every new note.
    cutoff_modulation: f64,
    tuning: f64,
    bend: f64,
    volume: f32,
    pressure: f64,
}

impl PluckVoice {
    fn new(params: &PluckParams) -> Self {
        let shape = AdsrParams {
            attack: 0.003,
            decay: f64::from(params.decay.value()),
            sustain: 0.3,
            release: f64::from(params.release.value()),
        };
        Self {
            oscillator: Oscillator::new(Waveform::Saw, 220.0),
            filter: StateVariableFilter::new(
                SvfMode::Lowpass,
                f64::from(params.cutoff.value()),
                f64::from(params.resonance.value()),
            ),
            envelope: Adsr::new(shape),
            shape,
            key: 60.0,
            velocity: 0.0,
            cutoff: f64::from(params.cutoff.value()),
            cutoff_modulation: 0.0,
            tuning: 0.0,
            bend: 0.0,
            volume: 1.0,
            pressure: 0.0,
        }
    }

    fn update_pitch(&mut self) {
        self.oscillator
            .set_frequency(note_to_hz(self.key + self.tuning + self.bend));
    }
}

impl Voice for PluckVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.oscillator.set_sample_rate(spec.sample_rate);
        self.envelope.set_sample_rate(spec.sample_rate);
        // The voice is mono inside and is copied to every output channel.
        self.filter.prepare(&ProcessSpec {
            num_channels: 1,
            ..*spec
        });
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        self.key = f64::from(note.key);
        self.velocity = velocity;
        self.update_pitch();
        self.envelope.note_on();
    }

    fn note_off(&mut self, _velocity: f32) {
        self.envelope.note_off();
    }

    fn choke(&mut self) {
        self.envelope.silence();
        self.filter.reset();
        self.oscillator.reset_phase();
        self.cutoff_modulation = 0.0;
        self.tuning = 0.0;
        self.volume = 1.0;
        self.pressure = 0.0;
    }

    fn is_active(&self) -> bool {
        self.envelope.is_active()
    }

    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        if !self.envelope.is_active() {
            return;
        }
        // Pressure opens the filter by up to two octaves.
        let base = (self.cutoff + self.cutoff_modulation) * 2f64.powf(2.0 * self.pressure);
        for frame in 0..buffer.num_frames() {
            let level = self.envelope.next_sample();
            self.filter.set_frequency(base * (0.25 + 0.75 * level));
            let raw = self.oscillator.next_sample() as f32;
            let sample =
                self.filter.process_sample(0, raw) * level as f32 * self.velocity * self.volume;
            for channel in buffer.iter_channels_mut() {
                channel[frame] += sample;
            }
        }
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        match expression {
            Expression::Tuning => {
                self.tuning = value;
                self.update_pitch();
            }
            Expression::Volume => self.volume = value as f32,
            Expression::Pressure | Expression::Brightness => self.pressure = value.clamp(0.0, 1.0),
            _ => {}
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        self.bend = semitones;
        self.update_pitch();
    }

    fn set_param(&mut self, param: ParamId, value: f64) {
        match param {
            CUTOFF => self.cutoff = value,
            RESONANCE => self.filter.set_q(value),
            DECAY => {
                self.shape.decay = value;
                self.envelope.set_params(self.shape);
            }
            RELEASE => {
                self.shape.release = value;
                self.envelope.set_params(self.shape);
            }
            _ => {}
        }
    }

    fn modulate(&mut self, param: ParamId, amount: f64) {
        if param == CUTOFF {
            self.cutoff_modulation = amount;
        }
    }
}

/// The plugin.
pub struct Pluck {
    params: Arc<PluckParams>,
    synth: Synth<PluckVoice>,
    reverb: Reverb<f32>,
    gain: Gain<f32>,
}

impl Pluck {
    /// The plugin's editor: the plugin's name and a knob for the output
    /// level, fixed at [`EDITOR_SIZE`] logical pixels. [`Plugin::editor`]
    /// returns the same, boxed.
    ///
    /// ```
    /// use mangue_pluck::{EDITOR_SIZE, Pluck};
    /// use manguebyte::plugin::Plugin;
    /// use manguebyte::plugin::editor::{EditorSpec, HostSize};
    ///
    /// let spec = Pluck::new().editor_spec();
    /// assert_eq!(spec.size(1.0), EDITOR_SIZE);
    /// assert_eq!(spec.size(2.0), HostSize::new(360, 300));
    /// ```
    pub fn editor_spec(&self) -> Editor {
        let params = Arc::clone(&self.params);
        Editor::new(EDITOR_SIZE, move || {
            column((
                label("Mangue Pluck").title(),
                knob(ParamHandle::new(Arc::clone(&params), |p: &PluckParams| {
                    &p.level
                })),
            ))
            .spacing(8.0)
            .padding(12.0)
        })
    }
}

impl Plugin for Pluck {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.pluck",
        name: "Mangue Pluck",
        vendor: "Manguebyte",
        version: env!("CARGO_PKG_VERSION"),
        description: "A polyphonic plucked synth, as an example of the Manguebyte framework",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    type Params = PluckParams;

    fn new() -> Self {
        let params = Arc::new(PluckParams::new());
        Self {
            synth: Synth::with_voices(16, || PluckVoice::new(&params)),
            reverb: Reverb::new().with_decay_seconds(2.2).with_damping(0.45),
            gain: Gain::from_db(f64::from(params.level.value())),
            params,
        }
    }

    fn params(&self) -> Arc<PluckParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, spec: &ProcessSpec) {
        self.synth.prepare(spec);
        self.reverb.prepare(spec);
        self.gain.prepare(spec);
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        // The synth follows the events, so notes and automation of the
        // voice parameters land on their exact frame.
        self.synth.process_with(buffer, context);

        // The effects after it read their parameters once per block. The
        // gain glides to its new value, so this never clicks.
        self.reverb.set_mix(f64::from(self.params.reverb.value()));
        self.reverb.process(buffer);
        self.gain.set_gain_db(f64::from(self.params.level.value()));
        self.gain.process(buffer);
    }

    fn reset(&mut self) {
        self.synth.reset();
        self.reverb.reset();
        self.gain.reset();
    }

    fn editor(&self) -> Option<Box<dyn EditorSpec>> {
        Some(Box::new(self.editor_spec()))
    }
}

manguebyte::clap::export_clap!(Pluck);
manguebyte::vst3::export_vst3!(Pluck);

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    /// Root-mean-square level of one stretch of the left channel.
    fn level(buffer: &AudioBuffer<f32>, range: std::ops::Range<usize>) -> f64 {
        let slice = &buffer.channel(0)[range];
        (slice
            .iter()
            .map(|&s| f64::from(s) * f64::from(s))
            .sum::<f64>()
            / slice.len() as f64)
            .sqrt()
    }

    #[test]
    fn is_silent_until_played_and_fades_after_release() {
        let mut harness = Harness::<Pluck>::new(RATE, 256);
        harness.params().reverb.set(0.0);
        assert_eq!(harness.render(4_800).peak(), 0.0);

        harness.note(harness.position(), 12_000, 57, 0.9);
        let sound = harness.render(48_000);
        assert!(level(&sound, 1_000..11_000) > 0.01);
        // Half a second after the release, with no reverb, it has faded.
        assert!(level(&sound, 40_000..48_000) < 1e-4);
        assert!(sound.peak() < 1.0, "the default level clips");
        assert_eq!(sound.channel(0), sound.channel(1));
    }

    #[test]
    fn plays_the_pitch_it_is_asked_for() {
        use manguebyte::dsp::fft::{Complex, RealFft};

        let mut harness = Harness::<Pluck>::new(RATE, 512);
        harness.params().reverb.set(0.0);
        // No resonant peak, so the fundamental is the loudest partial.
        harness.params().resonance.set(0.7);
        harness.note(0, 40_000, 69, 1.0); // A4, 440 Hz
        let sound = harness.render(32_768);

        let mut fft = RealFft::<f32>::new(32_768);
        let mut spectrum = vec![Complex::ZERO; fft.spectrum_len()];
        fft.forward(sound.channel(0), &mut spectrum);
        let loudest = (1..spectrum.len())
            .max_by(|&a, &b| spectrum[a].norm_sqr().total_cmp(&spectrum[b].norm_sqr()))
            .unwrap();
        let hertz = loudest as f64 * RATE / 32_768.0;
        assert!((hertz - 440.0).abs() < 2.0, "loudest partial at {hertz} Hz");
    }

    #[test]
    fn sounds_the_same_whatever_the_block_size() {
        let render = |max: usize, pattern: &[usize]| {
            let mut harness = Harness::<Pluck>::new(RATE, max).with_block_pattern(pattern);
            harness.note(100, 9_000, 45, 0.8);
            harness.note(4_000, 9_000, 52, 0.7);
            harness.note(4_001, 2_000, 57, 0.6);
            harness.automate(2_500, "cutoff", 4_000.0);
            harness.automate(7_777, "resonance", 5.0);
            harness.automate(9_000, "release", 0.05);
            harness.render(24_000)
        };
        let reference = render(256, &[256]);
        assert!(reference.peak() > 0.01);
        assert!(render(64, &[64]) == reference);
        assert!(render(1_024, &[1, 999, 13, 1_024]) == reference);
    }

    #[test]
    fn automation_and_per_note_modulation_change_the_sound() {
        let brightness = |prepare: &dyn Fn(&mut Harness<Pluck>)| {
            let mut harness = Harness::<Pluck>::new(RATE, 256);
            harness.params().reverb.set(0.0);
            harness.note(0, 20_000, 45, 1.0);
            prepare(&mut harness);
            let sound = harness.render(8_192);
            // Energy of the sample-to-sample difference: more of it means a
            // brighter sound.
            let left = sound.channel(0);
            left.windows(2)
                .map(|w| f64::from(w[1] - w[0]).powi(2))
                .sum::<f64>()
        };
        let plain = brightness(&|_| {});
        let automated = brightness(&|harness| harness.automate(0, "cutoff", 10_000.0));
        let modulated = brightness(&|harness| {
            harness.schedule(
                0,
                EventKind::ParamModulation {
                    param: CUTOFF,
                    amount: 8_000.0,
                    note: Some(NoteRef::new(0, 45)),
                },
            );
        });
        assert!(automated > 3.0 * plain);
        assert!(modulated > 3.0 * plain);
    }

    #[test]
    fn state_restores_every_parameter() {
        let harness = Harness::<Pluck>::new(RATE, 256);
        harness.params().cutoff.set(5_000.0);
        harness.params().level.set(-3.0);
        let saved = harness.save_state();

        let fresh = Harness::<Pluck>::new(RATE, 256);
        assert!(fresh.load_state(&saved).unwrap().is_exact());
        assert_eq!(fresh.params().cutoff.value(), 5_000.0);
        assert_eq!(fresh.params().level.value(), -3.0);
    }
}
