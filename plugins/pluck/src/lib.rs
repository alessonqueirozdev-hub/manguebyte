//! Mangue Pluck: an example instrument built with Manguebyte.
//!
//! A polyphonic plucked synth: each note is a sawtooth through a resonant
//! low-pass filter that closes as the note fades, and the whole instrument
//! goes through a reverb and a limiter. It is small on purpose. Its job is
//! to show a complete plugin in one file:
//!
//! - parameters with ranges, units and smoothing;
//! - a [`Voice`] driven by a [`Synth`], with notes on their exact frame;
//! - per-note expression (tuning, volume, pressure) and per-note modulation
//!   of the filter;
//! - sample-accurate automation, for the voices and for the effects after
//!   them, so that the output is the same, bit for bit, however a host cuts
//!   the timeline into blocks, and after any reset;
//! - notes that stay in tune while their filter sweeps;
//! - an output kept in range with every setting at its maximum: the voices
//!   share the headroom, a soft knee bends what rises past
//!   [`OUTPUT_KNEE_DB`] smoothly towards [`OUTPUT_CEILING_DB`] without
//!   flattening it there, and a limiter guarantees the ceiling for
//!   anything else;
//! - one line each to export it as a CLAP plugin and as a VST 3 plugin,
//!   from the same library;
//! - a small editor: a title and a knob for each parameter (see
//!   [`Pluck::editor_spec`]). Mangue Duck's also has a meter and a switch.
//!
//! Run `scripts/bundle-clap.sh mangue-pluck "Mangue Pluck"` to get a `.clap`
//! file a host can load, and `scripts/bundle-vst3.sh mangue-pluck
//! "Mangue Pluck"` to get a `.vst3` bundle.

use std::sync::Arc;

use manguebyte::plugin::editor::{EditorSpec, HostSize};
use manguebyte::prelude::*;
use manguebyte_gui::Editor;
use manguebyte_ui::{IntoView, ParamHandle, column, knob, label, row};

/// The editor's size at scale 1, in logical pixels.
pub const EDITOR_SIZE: HostSize = HostSize::new(520, 160);

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
    /// Output level in decibels: a plain gain, ahead of the output's soft
    /// knee. Up to -6 dB, one note at full velocity stays under the knee at
    /// every setting of the other parameters measured (see
    /// `tests/range.rs`), so the level is exactly what it says. Above that,
    /// very resonant or dense playing goes into the knee, which bends it
    /// towards the ceiling.
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
const REVERB: ParamId = ParamId::from_name("reverb");
const LEVEL: ParamId = ParamId::from_name("level");

/// The level no output sample exceeds, in decibels relative to full scale.
pub const OUTPUT_CEILING_DB: f64 = -1.0;

/// Where the output's soft knee begins, in decibels relative to full scale.
/// Below it a sample passes unchanged. Above it the curve bends smoothly
/// (its slope is continuous) towards [`OUTPUT_CEILING_DB`] and only comes
/// near it for samples far louder than the ceiling: a sample 20 dB over the
/// ceiling comes out less than 0.1 dB under it. The loudest single note
/// measured at the level's -6 dB peaks at about -4 dBFS, under the knee. This is a gentle saturation of
/// the loudest peaks, not a clip, and nothing comes out flat at the
/// ceiling (see `tests/range.rs`).
pub const OUTPUT_KNEE_DB: f64 = -3.0;

/// How far ahead the output limiter looks, in milliseconds: not at all. A
/// look-ahead would make the output late by as much, and an instrument that
/// is played live should not be. The soft knee ahead of it keeps every
/// finite sample under the ceiling, so the limiter only stands guard: it
/// would act on a sample the knee let through by rounding, and it turns a
/// sample that is not a finite number into silence.
const LIMITER_LOOKAHEAD_MS: f64 = 0.0;

/// How quickly the limiter lets go after a peak, in milliseconds.
const LIMITER_RELEASE_MS: f64 = 150.0;

/// Each voice's share of the output. With it, one note at full velocity
/// and the level at -6 dB peaks under the soft knee at every setting
/// measured (see the test `up_to_minus_6_db_one_note_is_never_shaped`),
/// and at the default level, -12 dB, a note at the default settings peaks
/// at -19 to -22 dBFS, depending on the key, leaving room for chords. Notes
/// that start together are correlated and can add up to more than full
/// scale; the knee takes care of that.
const VOICE_GAIN: f32 = 0.5;

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

/// The highest offset in semitones a note can be retuned by, up or down:
/// CLAP's range for its tuning expression.
const MAX_TUNING: f64 = 120.0;

/// The loudest a note's volume expression can make it: CLAP's range.
const MAX_VOLUME: f64 = 4.0;

/// How long the voices take to follow a change of the resonance's
/// loudness.
const RESONANCE_GLIDE_SECONDS: f64 = 0.02;

/// How long the reverb's blend takes to follow its parameter.
const EFFECT_GLIDE_SECONDS: f64 = 0.02;

/// How loud the reverb's own sound is blended in. The reverb rings for
/// seconds, so a held note builds up in it to several times its dry peak;
/// this brings the fully wet sound back to about the loudness of the dry
/// one, measured on held notes.
const WET_TRIM: f32 = 0.37;

/// The gain that makes up for the filter's resonance. The filter peaks by
/// its Q at the cutoff, and the cutoff sweeps across the note's harmonics,
/// so above a Q of 1 the loudest moment of a note grows with the Q: by
/// about Q^0.63, measured on single notes over the range of every setting.
/// This takes most of that back, so the resonance changes the sound more
/// than the level.
fn resonance_gain(q: f64) -> f32 {
    if q > 1.0 { q.powf(-0.6) as f32 } else { 1.0 }
}

/// Below this many radians, an angle is taken from the first terms of its
/// tangent's series, which are exact to the last bit of an `f64` there.
/// Larger angles, which only a jump of the cutoff gives, use `atan2`.
const SMALL_ANGLE: f64 = 0.01;

/// The angle of the complex number `(x, y)`, cheaply when it is small.
fn angle_of(x: f64, y: f64) -> f64 {
    if x > 0.0 && y.abs() < SMALL_ANGLE * x {
        let t = y / x;
        let t2 = t * t;
        // atan(t) = t - t³/3 + t⁵/5 - ..., left out from t⁷/7 < 2e-15 t.
        t * (1.0 - t2 * (1.0 / 3.0 - t2 / 5.0))
    } else {
        y.atan2(x)
    }
}

/// One note.
struct PluckVoice {
    /// The parameters, to read events' values as they hold them.
    params: Arc<PluckParams>,
    oscillator: Oscillator,
    filter: StateVariableFilter<f32>,
    envelope: Adsr,
    shape: AdsrParams,
    sample_rate: f64,
    key: f64,
    /// The pitch the note should sound at, in hertz.
    pitch: f64,
    /// `tan(π·pitch / sample rate)`, the pitch as the filter's response
    /// formula takes it (see [`StateVariableFilter::coefficients`]),
    /// worked out once per pitch rather than on every frame.
    prewarped_pitch: f64,
    /// The denominator of the filter's response at `pitch` on the last
    /// frame, `1 - w² + i·k·w`, whose angle is the filter's lag there; or
    /// `None` when the pitch has just changed and there is no lag to follow
    /// yet.
    lag: Option<(f64, f64)>,
    velocity: f32,
    cutoff: f64,
    /// What makes up for the resonance's peak (see [`resonance_gain`]),
    /// gliding when the resonance changes so that it never clicks.
    resonance_gain: Smoothed<f32>,
    /// Per-note offsets, reset for every new note.
    cutoff_modulation: f64,
    tuning: f64,
    bend: f64,
    volume: f32,
    pressure: f64,
}

impl PluckVoice {
    fn new(params: &Arc<PluckParams>) -> Self {
        let shape = AdsrParams {
            attack: 0.003,
            decay: f64::from(params.decay.value()),
            sustain: 0.3,
            release: f64::from(params.release.value()),
        };
        let mut voice = Self {
            params: Arc::clone(params),
            oscillator: Oscillator::new(Waveform::Saw, 220.0),
            filter: StateVariableFilter::new(
                SvfMode::Lowpass,
                f64::from(params.cutoff.value()),
                f64::from(params.resonance.value()),
            ),
            envelope: Adsr::new(shape),
            shape,
            sample_rate: 44_100.0,
            key: 60.0,
            pitch: note_to_hz(60.0),
            prewarped_pitch: 0.0,
            lag: None,
            velocity: 0.0,
            cutoff: f64::from(params.cutoff.value()),
            resonance_gain: Smoothed::new(resonance_gain(f64::from(params.resonance.value()))),
            cutoff_modulation: 0.0,
            tuning: 0.0,
            bend: 0.0,
            volume: 1.0,
            pressure: 0.0,
        };
        voice.update_pitch();
        voice
    }

    fn update_pitch(&mut self) {
        self.pitch = note_to_hz(self.key + self.tuning + self.bend);
        self.oscillator.set_frequency(self.pitch);
        // Held just below the Nyquist frequency, as the filter's own
        // `phase_response` holds it.
        let nyquist = 0.5 * self.sample_rate;
        let pitch = self.pitch.clamp(0.0, nyquist * 0.999_999);
        self.prewarped_pitch = (core::f64::consts::PI * pitch / self.sample_rate).tan();
        // A new pitch has a new lag. What has to be made up for is the
        // filter's movement from here on, not the jump.
        self.lag = None;
    }
}

impl Voice for PluckVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.oscillator.set_sample_rate(spec.sample_rate);
        self.envelope.set_sample_rate(spec.sample_rate);
        self.resonance_gain
            .set_ramp(spec.sample_rate, RESONANCE_GLIDE_SECONDS);
        // The voice is mono inside and is copied to every output channel.
        self.filter.prepare(&ProcessSpec {
            num_channels: 1,
            ..*spec
        });
        self.update_pitch();
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
        self.lag = None;
        self.resonance_gain
            .set_immediate(self.resonance_gain.target());
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
        let hertz_per_radian = self.sample_rate / core::f64::consts::TAU;
        for frame in 0..buffer.num_frames() {
            let level = self.envelope.next_sample();
            self.filter.set_frequency(base * (0.25 + 0.75 * level));
            // As the filter closes, its delay at the note's pitch grows, and
            // a growing delay lowers the pitch that comes out while the note
            // decays: by up to 3 cents at the default settings, and 7.5 with
            // the resonance at its top. The oscillator runs ahead by the lag
            // the filter's response at the pitch gained since the last
            // frame, so the note leaves the filter at its own pitch: within
            // a cent from key 36 to 96 while the cutoff stays above the
            // note's fundamental. When a resonant filter sweeps across a low
            // note's fundamental faster than it settles, the response of the
            // filter as it stands is no longer what the note hears, and the
            // note is a few cents off while it passes (see `tests/tuning.rs`).
            //
            // The lag is the angle of the response's denominator, and what
            // it gained is the angle of the ratio of two of them, which
            // needs no tangent and, for the small steps of a sweep, no
            // arctangent either.
            let (g, k) = self.filter.coefficients();
            let w = self.prewarped_pitch / g;
            let denominator = (1.0 - w * w, k * w);
            let gained = self.lag.map_or(0.0, |last: (f64, f64)| {
                angle_of(
                    denominator.0 * last.0 + denominator.1 * last.1,
                    denominator.1 * last.0 - denominator.0 * last.1,
                )
            });
            self.lag = Some(denominator);
            self.oscillator
                .set_frequency(self.pitch + gained * hertz_per_radian);
            let raw = self.oscillator.next_sample() as f32;
            let sample = self.filter.process_sample(0, raw)
                * level as f32
                * self.velocity
                * self.volume
                * self.resonance_gain.next_value()
                * VOICE_GAIN;
            for channel in buffer.iter_channels_mut() {
                channel[frame] += sample;
            }
        }
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        // A value that is not a number is a fault, not a setting. The
        // others are kept to the ranges CLAP gives them.
        if value.is_nan() {
            return;
        }
        match expression {
            Expression::Tuning => {
                self.tuning = value.clamp(-MAX_TUNING, MAX_TUNING);
                self.update_pitch();
            }
            Expression::Volume => self.volume = value.clamp(0.0, MAX_VOLUME) as f32,
            Expression::Pressure | Expression::Brightness => self.pressure = value.clamp(0.0, 1.0),
            _ => {}
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        if semitones.is_nan() {
            return;
        }
        self.bend = semitones.clamp(-MAX_TUNING, MAX_TUNING);
        self.update_pitch();
    }

    fn set_param(&mut self, param: ParamId, value: f64) {
        // The value as the parameter holds it (see `Param::stored_plain`):
        // limited to the range and rounded to an `f32`, which is what the
        // voices start from, so a value sent again changes nothing. A value
        // that is not a number is ignored, as the parameter ignores it.
        let params = &*self.params;
        let held = match param {
            CUTOFF => params.cutoff.stored_plain(value),
            RESONANCE => params.resonance.stored_plain(value),
            DECAY => params.decay.stored_plain(value),
            RELEASE => params.release.stored_plain(value),
            _ => None,
        };
        let Some(value) = held else {
            return;
        };
        match param {
            CUTOFF => self.cutoff = value,
            RESONANCE => {
                self.filter.set_q(value);
                self.resonance_gain.set_target(resonance_gain(value));
            }
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
        if param == CUTOFF && amount.is_finite() {
            // No further than the whole range, either way.
            let span = f64::from(self.params.cutoff.range().max());
            self.cutoff_modulation = amount.clamp(-span, span);
        }
    }
}

/// The output's soft knee (see [`OUTPUT_KNEE_DB`]): unchanged up to `knee`,
/// then `knee + (ceiling - knee) * u / (1 + u)`, where `u` is how far the
/// sample is past the knee in units of `ceiling - knee`. Its value and its
/// slope are continuous at the knee, it rises all the way, and it stays
/// under `ceiling` for every finite sample (once rounded to an `f32`, it
/// can reach the ceiling itself, for samples more than 100 dB over full scale).
/// An infinity comes out at the ceiling and a NaN as a NaN, which the
/// limiter after it silences.
fn soft_knee(sample: f32, knee: f64, ceiling: f64) -> f32 {
    let size = f64::from(sample).abs();
    if size <= knee {
        return sample;
    }
    let room = ceiling - knee;
    let past = (size - knee) / room;
    // u / (1 + u), written so that an infinite u gives 1.
    let bent = knee + room * (1.0 - 1.0 / (1.0 + past));
    (bent as f32).copysign(sample)
}

/// What comes after the voices: the reverb, blended in by its parameter,
/// the level, the soft knee, and the limiter that keeps the output in
/// range.
///
/// Everything here follows its parameters' events on their exact frames,
/// as the voices do: reading the parameters once per block would make the
/// sound depend on where the host's blocks start.
struct Effects {
    reverb: Reverb<f32>,
    /// The reverb's own output, to blend with the dry sound.
    wet: AudioBuffer<f32>,
    /// How much of the reverb is blended in, gliding so that it never
    /// clicks.
    mix: Smoothed<f32>,
    gain: Gain<f32>,
    /// The soft knee's corners, as gains.
    knee: f64,
    ceiling: f64,
    limiter: Limiter<f32>,
    /// Whether any frame has gone through since the plugin was prepared or
    /// reset.
    started: bool,
}

impl Effects {
    fn new(params: &PluckParams) -> Self {
        Self {
            // The reverb gives its wet sound alone; `mix` blends it.
            reverb: Reverb::new()
                .with_decay_seconds(2.2)
                .with_damping(0.45)
                .with_mix(1.0),
            wet: AudioBuffer::new(0, 0),
            mix: Smoothed::new(params.reverb.value()),
            gain: Gain::from_db(f64::from(params.level.value())),
            knee: decibels::db_to_gain(OUTPUT_KNEE_DB),
            ceiling: decibels::db_to_gain(OUTPUT_CEILING_DB),
            limiter: Limiter::new()
                .with_ceiling_db(OUTPUT_CEILING_DB)
                .with_lookahead_ms(LIMITER_LOOKAHEAD_MS)
                .with_release_ms(LIMITER_RELEASE_MS),
            started: false,
        }
    }

    fn prepare(&mut self, spec: &ProcessSpec, params: &PluckParams) {
        self.reverb.prepare(spec);
        self.wet = AudioBuffer::new(spec.num_channels, spec.max_block_size);
        self.mix.set_ramp(spec.sample_rate, EFFECT_GLIDE_SECONDS);
        self.gain.prepare(spec);
        self.limiter.prepare(spec);
        self.start_where_the_parameters_are(params);
    }

    fn reset(&mut self, params: &PluckParams) {
        self.reverb.reset();
        self.limiter.reset();
        self.start_where_the_parameters_are(params);
    }

    /// Sets the reverb's blend and the level to their parameters' values
    /// at once, with no glide. Before the first frame since the plugin was
    /// prepared or reset there is nothing to click, and the sound starts
    /// where the parameters stand, so a render after a reset is the same as
    /// the first.
    fn start_where_the_parameters_are(&mut self, params: &PluckParams) {
        self.mix.set_immediate(params.reverb.value());
        self.gain.set_gain_db(f64::from(params.level.value()));
        self.gain.reset();
        self.started = false;
    }

    /// Follows one event, on its frame.
    fn handle(&mut self, params: &PluckParams, event: &Event) {
        let EventKind::ParamValue { param, value } = event.kind else {
            return;
        };
        // As the parameter holds it, and nothing if it is not a number, as
        // for the voices. Before the first frame, a change is a jump, for
        // the same reason as in `start_where_the_parameters_are`: a render
        // whose values arrive on its first frame must sound the same as
        // one whose values were set before it started.
        if param == REVERB {
            if let Some(mix) = params.reverb.stored_plain(value) {
                if self.started {
                    self.mix.set_target(mix as f32);
                } else {
                    self.mix.set_immediate(mix as f32);
                }
            }
        } else if param == LEVEL {
            if let Some(db) = params.level.stored_plain(value) {
                self.gain.set_gain_db(db);
                if !self.started {
                    self.gain.reset();
                }
            }
        }
    }

    /// Blends in the reverb and applies the level and the soft knee, in
    /// place. The limiter runs on the whole block afterwards.
    fn render(&mut self, piece: &mut AudioBuffer<f32>) {
        let frames = piece.num_frames();
        let room = self.wet.capacity();
        if room > 0 {
            let mut done = 0;
            while done < frames {
                let count = room.min(frames - done);
                piece.windowed(done..done + count, |part| self.blend_reverb(part));
                done += count;
            }
        }
        self.gain.process(piece);
        let (knee, ceiling) = (self.knee, self.ceiling);
        for channel in piece.iter_channels_mut() {
            for sample in channel {
                *sample = soft_knee(*sample, knee, ceiling);
            }
        }
        self.started = true;
    }

    fn blend_reverb(&mut self, dry: &mut AudioBuffer<f32>) {
        let frames = dry.num_frames();
        let channels = dry.num_channels().min(self.wet.num_channels());
        self.wet.set_num_frames(frames);
        for channel in 0..channels {
            self.wet
                .channel_mut(channel)
                .copy_from_slice(dry.channel(channel));
        }
        self.reverb.process(&mut self.wet);
        for frame in 0..frames {
            let mix = self.mix.next_value();
            for channel in 0..channels {
                let wet = self.wet.channel(channel)[frame];
                let sample = &mut dry.channel_mut(channel)[frame];
                *sample = *sample * (1.0 - mix) + wet * (mix * WET_TRIM);
            }
        }
    }
}

/// The plugin.
pub struct Pluck {
    params: Arc<PluckParams>,
    synth: Synth<PluckVoice>,
    effects: Effects,
}

impl Pluck {
    /// The plugin's editor: the plugin's name and a knob for each of its
    /// six parameters, in the order a host lists them, fixed at
    /// [`EDITOR_SIZE`] logical pixels. [`Plugin::editor`] returns the same,
    /// boxed.
    ///
    /// ```
    /// use mangue_pluck::{EDITOR_SIZE, Pluck};
    /// use manguebyte::plugin::Plugin;
    /// use manguebyte::plugin::editor::{EditorSpec, HostSize};
    ///
    /// let spec = Pluck::new().editor_spec();
    /// assert_eq!(spec.size(1.0), EDITOR_SIZE);
    /// assert_eq!(spec.size(2.0), HostSize::new(1_040, 320));
    /// ```
    pub fn editor_spec(&self) -> Editor {
        let params = Arc::clone(&self.params);
        Editor::new(EDITOR_SIZE, move || {
            let param = |field: fn(&PluckParams) -> &FloatParam| {
                knob(ParamHandle::new(Arc::clone(&params), field)).flex(1.0)
            };
            column((
                label("Mangue Pluck").title(),
                row((
                    param(|p| &p.cutoff),
                    param(|p| &p.resonance),
                    param(|p| &p.decay),
                    param(|p| &p.release),
                    param(|p| &p.reverb),
                    param(|p| &p.level),
                ))
                .spacing(8.0)
                .flex(1.0),
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
            effects: Effects::new(&params),
            params,
        }
    }

    fn params(&self) -> Arc<PluckParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, spec: &ProcessSpec) {
        self.synth.prepare(spec);
        self.effects.prepare(spec, &self.params);
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        // The synth follows the events, so notes and automation of the
        // voice parameters land on their exact frame. So do the effects.
        self.synth.process_with(buffer, context);
        let params = &*self.params;
        split_at_events(
            &mut self.effects,
            buffer,
            context.events,
            |effects, event| effects.handle(params, event),
            Effects::render,
        );
        self.effects.limiter.process(buffer);
    }

    fn reset(&mut self) {
        self.synth.reset();
        self.effects.reset(&self.params);
    }

    fn latency_samples(&self) -> usize {
        self.effects.limiter.latency_samples()
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

    /// TECH_DEBT 42: REAPER renders of many overlapping notes differed by
    /// one unit in the last place. More notes than voices, so that voices
    /// are stolen, the same key struck again while it sounds, and block
    /// lengths that change all the time, as a host's do.
    #[test]
    fn many_overlapping_notes_sound_the_same_whatever_the_block_size() {
        let render = |max: usize, pattern: &[usize]| {
            let mut harness = Harness::<Pluck>::new(RATE, max).with_block_pattern(pattern);
            let mut start = 0_u64;
            for index in 0..60_u64 {
                let key = 36 + ((index * 7) % 37) as u8;
                let velocity = 0.3 + 0.7 * ((index * 13) % 10) as f32 / 10.0;
                harness.note(start, 3_000 + (index * 977) % 20_000, key, velocity);
                // Chords: some notes start on the same frame.
                start += [0, 0, 211, 1_009, 37, 0, 2_503][(index % 7) as usize];
            }
            harness.automate(5_000, "cutoff", 6_000.0);
            harness.automate(20_011, "release", 1.5);
            harness.render(72_000)
        };
        let reference = render(512, &[512]);
        assert!(reference.peak() > 0.05);
        for (max, pattern) in [
            (64, &[64][..]),
            (1_024, &[1, 999, 13, 1_024][..]),
            (512, &[7][..]),
            (512, &[512, 3, 77, 128, 511][..]),
        ] {
            let output = render(max, pattern);
            let first = (0..72_000).find(|&frame| {
                (0..2).any(|channel| {
                    output.channel(channel)[frame].to_bits()
                        != reference.channel(channel)[frame].to_bits()
                })
            });
            assert_eq!(first, None, "with blocks of {pattern:?}");
        }
    }

    /// Hosts send a parameter's value again, at the start of blocks or
    /// when a session is rendered, and where blocks start changes from one
    /// render to the next. A value sent again must change nothing, even
    /// one an `f32` cannot hold exactly (the default decay, 0.3 s, and
    /// release, 0.4 s), as `Harness::automate` passes it on. The wrappers
    /// already deliver values an `f32` holds, so this does not explain
    /// TECH_DEBT 42.
    #[test]
    fn values_sent_again_change_nothing() {
        let render = |again: Option<(&str, f64)>| {
            let mut harness = Harness::<Pluck>::new(RATE, 512);
            for index in 0..40_u64 {
                let key = 40 + ((index * 5) % 30) as u8;
                harness.note(index * 1_500, 4_000 + (index * 331) % 9_000, key, 0.8);
            }
            if let Some((name, value)) = again {
                for frame in (0..72_000).step_by(997) {
                    harness.automate(frame, name, value);
                }
            }
            harness.render(72_000)
        };
        let reference = render(None);
        assert!(reference.peak() > 0.05);
        for again in [
            ("cutoff", 1_800.0),
            ("resonance", 2.0),
            ("decay", 0.3),
            ("release", 0.4),
            ("reverb", 0.25),
            ("level", -12.0),
        ] {
            let output = render(Some(again));
            let first = (0..72_000).find(|&frame| {
                (0..2).any(|channel| {
                    output.channel(channel)[frame].to_bits()
                        != reference.channel(channel)[frame].to_bits()
                })
            });
            assert_eq!(first, None, "{again:?} sent again");
        }
    }

    /// A host that sends a position that is not a number gets it converted
    /// by the wrappers (`Param::normalized_to_plain`) to a value that is not
    /// a number either. The parameter refuses to store it, and the voices
    /// ignore it: the notes sound exactly as with no such event.
    #[test]
    fn parameter_values_that_are_not_a_number_change_nothing() {
        let render = |bad: Option<&str>| {
            let mut harness = Harness::<Pluck>::new(RATE, 256);
            harness.note(0, 20_000, 57, 0.9);
            harness.note(5_000, 10_000, 64, 0.7);
            if let Some(name) = bad {
                let param = harness.params().find(name).expect("a parameter");
                let kind = EventKind::ParamValue {
                    param: param.param_id(),
                    value: param.normalized_to_plain(f64::NAN),
                };
                assert!(matches!(kind, EventKind::ParamValue { value, .. } if value.is_nan()));
                for frame in [10, 4_999, 5_000, 12_345] {
                    harness.schedule(frame, kind);
                }
            }
            harness.render(24_000)
        };
        let reference = render(None);
        assert!(reference.peak() > 0.05);
        for name in ["cutoff", "resonance", "decay", "release", "reverb", "level"] {
            let output = render(Some(name));
            let first_bad = output.channel(0).iter().position(|s| !s.is_finite());
            assert_eq!(first_bad, None, "a NaN {name} reaches the output");
            assert!(output == reference, "a NaN {name} changed the sound");
        }
    }

    /// Hostile values in every event that carries a number, at random
    /// frames among overlapping notes: the output stays finite, and values
    /// beyond a parameter's range act as its nearer end.
    #[test]
    fn hostile_values_in_any_event_never_reach_the_speakers() {
        let values = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e300,
            -1e300,
            f64::MIN_POSITIVE,
            -0.0,
        ];
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move |n: u64| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
        };
        for round in 0..6 {
            let mut harness = Harness::<Pluck>::new(RATE, 256);
            for index in 0..12_u64 {
                harness.note(index * 1_700, 6_000, 45 + (index * 5 % 24) as u8, 0.8);
            }
            for _ in 0..40 {
                let frame = next(24_000);
                let value = values[next(values.len() as u64) as usize];
                let note = NoteRef::new(0, 45 + (next(24) as u8));
                let kind = match next(4) {
                    0 => EventKind::ParamValue {
                        param: [CUTOFF, RESONANCE, DECAY, RELEASE][next(4) as usize],
                        value,
                    },
                    1 => EventKind::ParamModulation {
                        param: CUTOFF,
                        amount: value,
                        note: Some(note),
                    },
                    _ => EventKind::NoteExpression {
                        note,
                        expression: [
                            Expression::Tuning,
                            Expression::Volume,
                            Expression::Pressure,
                            Expression::Brightness,
                        ][next(4) as usize],
                        value,
                    },
                };
                harness.schedule(frame, kind);
            }
            let output = harness.render(30_000);
            for channel in 0..2 {
                let bad = output.channel(channel).iter().position(|s| !s.is_finite());
                assert_eq!(bad, None, "round {round}, channel {channel}");
            }
        }
        // Beyond the range is the nearer end.
        let render = |name: &str, value: f64| {
            let mut harness = Harness::<Pluck>::new(RATE, 256);
            harness.note(0, 9_000, 57, 0.9);
            harness.automate(100, name, value);
            harness.render(12_000)
        };
        for (name, low, high) in [
            ("cutoff", 100.0, 12_000.0),
            ("resonance", 0.5, 8.0),
            ("decay", 0.01, 5.0),
            ("release", 0.01, 5.0),
        ] {
            assert!(render(name, f64::INFINITY) == render(name, high), "{name}");
            assert!(render(name, -1e300) == render(name, low), "{name}");
        }
    }

    /// A note whose velocity is not a finite number (a NaN, or what a CLAP
    /// host's velocity of 1e300 becomes once the wrapper makes it an `f32`)
    /// once put a NaN into the reverb's feedback, and every sample after it
    /// was a NaN, for good. Velocities are kept to 0..=1: one that is not a
    /// finite number, or below 0, plays as 0, and one above 1 as 1.
    #[test]
    fn hostile_velocities_never_reach_the_speakers() {
        let render = |velocity: f32| {
            let mut harness = Harness::<Pluck>::new(RATE, 512);
            let note = NoteRef::new(0, 60);
            harness.schedule(0, EventKind::NoteOn { note, velocity });
            harness.schedule(
                4_800,
                EventKind::NoteOff {
                    note,
                    velocity: 0.0,
                },
            );
            // A healthy note afterwards, long after the hostile one ended.
            harness.note(96_000, 4_800, 64, 0.8);
            harness.render(144_000)
        };
        let silent = render(0.0);
        let loudest = render(1.0);
        assert!(silent.channel(0)[96_000..].iter().any(|&s| s != 0.0));
        for (velocity, expected) in [
            (f32::NAN, &silent),
            (-f32::NAN, &silent),
            (f32::INFINITY, &silent),
            (f32::NEG_INFINITY, &silent),
            (-0.5, &silent),
            (f32::MAX, &loudest),
            (1.5, &loudest),
            (1e300_f64 as f32, &silent),
        ] {
            let sound = render(velocity);
            for channel in 0..2 {
                let bad = sound
                    .channel(channel)
                    .iter()
                    .filter(|s| !s.is_finite())
                    .count();
                assert_eq!(bad, 0, "velocity {velocity}, channel {channel}");
            }
            assert!(sound == *expected, "velocity {velocity}");
        }
    }

    #[test]
    fn the_soft_knee_is_smooth_and_stays_under_the_ceiling() {
        let knee = decibels::db_to_gain(OUTPUT_KNEE_DB);
        let ceiling = decibels::db_to_gain(OUTPUT_CEILING_DB);
        let shape = |x: f32| soft_knee(x, knee, ceiling);
        // Unchanged up to the knee, bit for bit, either way.
        for x in [0.0, -0.0, 1e-30, 0.25, -0.5, knee as f32, -(knee as f32)] {
            assert_eq!(shape(x).to_bits(), x.to_bits(), "{x}");
        }
        // Rising all the way, odd, and under the ceiling up to 100 dB over
        // full scale; beyond that, rounding to an `f32` can give the
        // ceiling itself, and never more.
        let mut last = 0.0_f32;
        let mut x = knee as f32;
        while x < 1e5 {
            let y = shape(x);
            assert!(y >= last && y < ceiling as f32, "{x} gives {y}");
            assert_eq!(shape(-x), -y);
            last = y;
            x *= 1.001;
        }
        for x in [1e6, 1e12, 1e30] {
            assert!(shape(x) <= ceiling as f32, "{x}");
        }
        assert_eq!(shape(f32::MAX), ceiling as f32);
        assert_eq!(shape(f32::INFINITY), ceiling as f32);
        assert!(shape(f32::NAN).is_nan());
        // The slope is continuous at the knee: 1 on both sides.
        let h = 1e-4_f32;
        let k = knee as f32;
        let above = (shape(k + h) - shape(k)) / h;
        assert!((above - 1.0).abs() < 1e-2, "slope {above} above the knee");
        // 20 dB over the ceiling comes out within 0.1 dB of it, and 3 dB
        // over it within 1 dB.
        let db = |x: f64| 20.0 * (f64::from(shape(x as f32)) / ceiling).log10();
        assert!(
            (-0.1..0.0).contains(&db(10.0 * ceiling)),
            "{}",
            db(10.0 * ceiling)
        );
        assert!(
            (-1.0..-0.1).contains(&db(1.41 * ceiling)),
            "{}",
            db(1.41 * ceiling)
        );
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
