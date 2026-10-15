//! The drum kit: six synthesised drums on the General MIDI note numbers.

use core::f64::consts::{LN_2, TAU};

use manguebyte_dsp::{Oscillator, ProcessSpec, Processor, StateVariableFilter, SvfMode, Waveform};
use manguebyte_params::ParamId;

use crate::params::{self, KitParams};
use crate::synth::{SILENT, decay_coefficient};

/// One drum of the kit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Drum {
    /// Bass drum: a sine that falls in pitch, with a click.
    Kick,
    /// Snare: two tuned sines and filtered noise.
    Snare,
    /// Closed hi-hat. Striking it chokes the open hi-hat.
    ClosedHat,
    /// Open hi-hat.
    OpenHat,
    /// Ride cymbal.
    Ride,
    /// Crash cymbal.
    Crash,
}

/// One row of the published drum map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrumNote {
    /// The drum.
    pub drum: Drum,
    /// Its General MIDI note number.
    pub note: u8,
    /// The symbol Aratu writes for it.
    pub symbol: char,
    /// Its name, as in the JSON map.
    pub name: &'static str,
}

/// The drum map: General MIDI note numbers and Aratu's symbols.
///
/// | Drum       | Note | Symbol |
/// |------------|------|--------|
/// | kick       | 36   | `b`    |
/// | snare      | 38   | `s`    |
/// | closed hat | 42   | `c`    |
/// | open hat   | 46   | `o`    |
/// | crash      | 49   | `x`    |
/// | ride       | 51   | `r`    |
///
/// A drum part ignores every other note.
pub const DRUM_MAP: [DrumNote; 6] = [
    DrumNote {
        drum: Drum::Kick,
        note: 36,
        symbol: 'b',
        name: "kick",
    },
    DrumNote {
        drum: Drum::Snare,
        note: 38,
        symbol: 's',
        name: "snare",
    },
    DrumNote {
        drum: Drum::ClosedHat,
        note: 42,
        symbol: 'c',
        name: "closed_hat",
    },
    DrumNote {
        drum: Drum::OpenHat,
        note: 46,
        symbol: 'o',
        name: "open_hat",
    },
    DrumNote {
        drum: Drum::Crash,
        note: 49,
        symbol: 'x',
        name: "crash",
    },
    DrumNote {
        drum: Drum::Ride,
        note: 51,
        symbol: 'r',
        name: "ride",
    },
];

impl Drum {
    /// Every drum, in the order the kit adds them up.
    pub const ALL: [Drum; 6] = [
        Drum::Kick,
        Drum::Snare,
        Drum::ClosedHat,
        Drum::OpenHat,
        Drum::Ride,
        Drum::Crash,
    ];

    fn entry(self) -> &'static DrumNote {
        // Every drum has exactly one row.
        let mut index = 0;
        while DRUM_MAP[index].drum as u8 != self as u8 {
            index += 1;
        }
        &DRUM_MAP[index]
    }

    /// The drum on General MIDI note `note`, if the kit has one there.
    ///
    /// ```
    /// use manguebyte_web::Drum;
    ///
    /// assert_eq!(Drum::from_note(42), Some(Drum::ClosedHat));
    /// assert_eq!(Drum::from_note(60), None);
    /// ```
    pub fn from_note(note: u8) -> Option<Self> {
        DRUM_MAP
            .iter()
            .find(|entry| entry.note == note)
            .map(|entry| entry.drum)
    }

    /// The drum Aratu writes as `symbol`.
    ///
    /// ```
    /// use manguebyte_web::Drum;
    ///
    /// assert_eq!(Drum::from_symbol('o').map(Drum::note), Some(46));
    /// ```
    pub fn from_symbol(symbol: char) -> Option<Self> {
        DRUM_MAP
            .iter()
            .find(|entry| entry.symbol == symbol)
            .map(|entry| entry.drum)
    }

    /// The General MIDI note number.
    pub fn note(self) -> u8 {
        self.entry().note
    }

    /// Aratu's symbol.
    pub fn symbol(self) -> char {
        self.entry().symbol
    }

    /// The name used in the JSON map and in parameter identifiers.
    pub fn name(self) -> &'static str {
        self.entry().name
    }

}

/// [`DRUM_MAP`] as JSON: an array of `{"name", "note", "symbol"}` objects
/// in the map's order. Allocates.
///
/// ```
/// let json = manguebyte_web::drum_map_json();
/// assert!(json.starts_with(r#"[{"name":"kick","note":36,"symbol":"b"},"#));
/// ```
pub fn drum_map_json() -> String {
    let rows: Vec<String> = DRUM_MAP
        .iter()
        .map(|entry| {
            format!(
                r#"{{"name":"{}","note":{},"symbol":"{}"}}"#,
                entry.name, entry.note, entry.symbol
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

/// Settings every drum reads when it is struck.
#[derive(Debug, Clone, Copy, PartialEq)]
struct KitSettings {
    /// Frequency ratio from the tune parameter.
    tune: f64,
    /// Factor on every decay time.
    decay: f64,
}

/// A level that falls by a fixed factor every sample, from 1.
#[derive(Debug, Clone, Copy)]
struct Shot {
    level: f64,
    coefficient: f64,
}

impl Shot {
    const SILENT: Self = Self {
        level: 0.0,
        coefficient: 0.0,
    };

    fn start(&mut self, seconds: f64, sample_rate: f64) {
        self.level = 1.0;
        self.coefficient = decay_coefficient(seconds, sample_rate);
    }

    fn next(&mut self) -> f64 {
        let level = self.level;
        self.level *= self.coefficient;
        level
    }

    fn sounding(&self) -> bool {
        self.level > SILENT
    }
}

/// White noise from a 64-bit xorshift generator (G. Marsaglia,
/// "Xorshift RNGs", Journal of Statistical Software, 2003).
#[derive(Debug, Clone, Copy)]
struct WhiteNoise(u64);

impl WhiteNoise {
    fn next(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// A fade to silence over a fixed number of frames, from wherever the
/// level is when it starts.
#[derive(Debug, Clone, Copy)]
struct Fade {
    gain: f64,
    step: f64,
    remaining: u32,
}

impl Fade {
    const OFF: Self = Self {
        gain: 1.0,
        step: 0.0,
        remaining: 0,
    };

    fn is_on(&self) -> bool {
        self.remaining > 0
    }

    /// Fades out over `frames`, unless a shorter fade is already running.
    fn start(&mut self, frames: u32) {
        let frames = frames.max(1);
        if !self.is_on() || frames < self.remaining {
            self.step = self.gain / f64::from(frames);
            self.remaining = frames;
        }
    }

    /// The gain for this frame, and whether the fade has ended after it.
    fn next(&mut self) -> (f64, bool) {
        let gain = self.gain;
        self.gain -= self.step;
        self.remaining -= 1;
        (gain, self.remaining == 0)
    }
}

trait Sound {
    fn prepare(&mut self, sample_rate: f64);
    fn strike(&mut self, kit: &KitSettings);
    fn next(&mut self) -> f64;
    fn sounding(&self) -> bool;
}

fn prepared_filter(mode: SvfMode, frequency: f64, q: f64, sample_rate: f64) -> StateVariableFilter<f64> {
    let mut filter = StateVariableFilter::new(mode, frequency, q);
    Processor::<f64>::prepare(
        &mut filter,
        &ProcessSpec {
            sample_rate,
            max_block_size: 1,
            num_channels: 1,
        },
    );
    filter
}

#[derive(Debug, Clone)]
struct Kick {
    sample_rate: f64,
    tune: f64,
    phase: f64,
    pitch: Shot,
    body: Shot,
    click: Shot,
    noise: WhiteNoise,
}

impl Sound for Kick {
    fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
    }

    fn strike(&mut self, kit: &KitSettings) {
        self.tune = kit.tune;
        self.phase = 0.0;
        self.pitch.start(0.12, self.sample_rate);
        self.body.start(0.6 * kit.decay, self.sample_rate);
        self.click.start(0.006, self.sample_rate);
    }

    fn next(&mut self) -> f64 {
        let frequency = self.tune * (45.0 + 135.0 * self.pitch.next());
        self.phase = (self.phase + frequency / self.sample_rate).fract();
        let body = (TAU * self.phase).sin() * self.body.next();
        let click = self.noise.next() * self.click.next();
        // A soft saturation thickens the body; at most 1 in, at most 1 out.
        let drive = 2.2;
        (drive * (0.85 * body + 0.15 * click)).tanh() / drive.tanh()
    }

    fn sounding(&self) -> bool {
        self.body.sounding()
    }
}

#[derive(Debug, Clone)]
struct Snare {
    sample_rate: f64,
    tune: f64,
    phases: [f64; 2],
    tone: Shot,
    rattle: Shot,
    noise: WhiteNoise,
    highpass: StateVariableFilter<f64>,
}

impl Sound for Snare {
    fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        self.highpass = prepared_filter(SvfMode::Highpass, 1_800.0, 0.7, sample_rate);
    }

    fn strike(&mut self, kit: &KitSettings) {
        self.tune = kit.tune;
        self.phases = [0.0, 0.0];
        self.tone.start(0.14 * kit.decay, self.sample_rate);
        self.rattle.start(0.24 * kit.decay, self.sample_rate);
        Processor::<f64>::reset(&mut self.highpass);
    }

    fn next(&mut self) -> f64 {
        for (phase, frequency) in self.phases.iter_mut().zip([185.0, 330.0]) {
            *phase = (*phase + self.tune * frequency / self.sample_rate).fract();
        }
        let body = (0.65 * (TAU * self.phases[0]).sin() + 0.35 * (TAU * self.phases[1]).sin())
            * self.tone.next();
        let hiss = self.highpass.process_sample(0, self.noise.next()) * self.rattle.next();
        0.5 * body + 0.4 * hiss
    }

    fn sounding(&self) -> bool {
        self.tone.sounding() || self.rattle.sounding()
    }
}

/// The six square waves of the classic analogue cymbal circuit, in hertz.
const METAL: [f64; 6] = [205.3, 304.4, 369.6, 522.7, 540.0, 800.0];

/// What makes one cymbal different from another.
#[derive(Debug, Clone, Copy)]
struct CymbalShape {
    /// Factor on the metal frequencies.
    spread: f64,
    band: f64,
    band_q: f64,
    high: f64,
    decay: f64,
    noise: f64,
    gain: f64,
}

const CLOSED_HAT: CymbalShape = CymbalShape {
    spread: 1.0,
    band: 10_000.0,
    band_q: 0.9,
    high: 7_000.0,
    decay: 0.07,
    noise: 0.4,
    gain: 1.2,
};

const OPEN_HAT: CymbalShape = CymbalShape {
    decay: 0.6,
    ..CLOSED_HAT
};

const RIDE: CymbalShape = CymbalShape {
    spread: 1.47,
    band: 5_200.0,
    band_q: 2.0,
    high: 2_600.0,
    decay: 1.8,
    noise: 0.1,
    gain: 2.0,
};

const CRASH: CymbalShape = CymbalShape {
    spread: 1.18,
    band: 6_500.0,
    band_q: 0.6,
    high: 3_600.0,
    decay: 2.4,
    noise: 0.8,
    gain: 0.6,
};

#[derive(Debug, Clone)]
struct Cymbal {
    shape: CymbalShape,
    sample_rate: f64,
    metal: [Oscillator; 6],
    band: StateVariableFilter<f64>,
    high: StateVariableFilter<f64>,
    amplitude: Shot,
    noise: WhiteNoise,
}

impl Cymbal {
    fn new(shape: CymbalShape, seed: u64) -> Self {
        Self {
            shape,
            sample_rate: 48_000.0,
            metal: METAL.map(|frequency| Oscillator::new(Waveform::Square, frequency)),
            band: StateVariableFilter::new(SvfMode::Bandpass, shape.band, shape.band_q),
            high: StateVariableFilter::new(SvfMode::Highpass, shape.high, 0.7),
            amplitude: Shot::SILENT,
            noise: WhiteNoise(seed),
        }
    }
}

impl Sound for Cymbal {
    fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        for oscillator in &mut self.metal {
            oscillator.set_sample_rate(sample_rate);
        }
        self.band = prepared_filter(SvfMode::Bandpass, self.shape.band, self.shape.band_q, sample_rate);
        self.high = prepared_filter(SvfMode::Highpass, self.shape.high, 0.7, sample_rate);
    }

    fn strike(&mut self, kit: &KitSettings) {
        for (oscillator, frequency) in self.metal.iter_mut().zip(METAL) {
            oscillator.set_frequency(frequency * self.shape.spread * kit.tune);
            oscillator.reset_phase();
        }
        Processor::<f64>::reset(&mut self.band);
        Processor::<f64>::reset(&mut self.high);
        self.amplitude
            .start(self.shape.decay * kit.decay, self.sample_rate);
    }

    fn next(&mut self) -> f64 {
        let metal: f64 = self.metal.iter_mut().map(Oscillator::next_sample).sum::<f64>() / 6.0;
        let source = metal + self.shape.noise * self.noise.next();
        let banded = self.band.process_sample(0, source);
        let bright = self.high.process_sample(0, banded);
        bright * self.amplitude.next() * self.shape.gain
    }

    fn sounding(&self) -> bool {
        self.amplitude.sounding()
    }
}

/// A drum and what is common to every drum: velocity, level and fades.
#[derive(Debug, Clone)]
struct Slot<D> {
    sound: D,
    sounding: bool,
    velocity: f64,
    level: f64,
    fade: Fade,
}

impl<D: Sound> Slot<D> {
    fn new(sound: D) -> Self {
        Self {
            sound,
            sounding: false,
            velocity: 0.0,
            level: 1.0,
            fade: Fade::OFF,
        }
    }

    fn strike(&mut self, velocity: f64, kit: &KitSettings) {
        self.sound.strike(kit);
        self.sounding = true;
        self.velocity = velocity;
        self.fade = Fade::OFF;
    }

    fn fade_out(&mut self, frames: u32) {
        if self.sounding {
            self.fade.start(frames);
        }
    }

    fn stop(&mut self) {
        self.sounding = false;
        self.fade = Fade::OFF;
    }

    fn next(&mut self) -> f64 {
        if !self.sounding {
            return 0.0;
        }
        let mut value = self.sound.next() * self.velocity * self.level;
        if self.fade.is_on() {
            let (gain, ended) = self.fade.next();
            value *= gain;
            if ended {
                self.stop();
            }
        }
        if !self.sound.sounding() {
            self.stop();
        }
        value
    }
}

/// How long the open hi-hat takes to fall silent when the closed one is
/// struck: a straight fade over `round(HAT_CHOKE_SECONDS × sample rate)`
/// frames, the first of them the frame the closed hat lands on.
pub const HAT_CHOKE_SECONDS: f64 = 0.002;

/// How long every sounding drum takes to fall silent when its part gets
/// all-notes-off: a straight fade over `round(DAMP_SECONDS × sample rate)`
/// frames. Drums are one-shots, so a note off does nothing to them.
pub const DAMP_SECONDS: f64 = 0.05;

/// A drum part's instrument.
#[derive(Debug, Clone)]
pub(crate) struct DrumKit {
    settings: KitSettings,
    kick: Slot<Kick>,
    snare: Slot<Snare>,
    hats: [Slot<Cymbal>; 2],
    cymbals: [Slot<Cymbal>; 2],
    choke_frames: u32,
    damp_frames: u32,
}

fn frames_for(seconds: f64, sample_rate: f64) -> u32 {
    (seconds * sample_rate).round().max(1.0) as u32
}

impl DrumKit {
    pub(crate) fn new(params: &KitParams, sample_rate: f64) -> Self {
        let mut kit = Self {
            settings: KitSettings {
                tune: 1.0,
                decay: 1.0,
            },
            kick: Slot::new(Kick {
                sample_rate,
                tune: 1.0,
                phase: 0.0,
                pitch: Shot::SILENT,
                body: Shot::SILENT,
                click: Shot::SILENT,
                noise: WhiteNoise(0x9E37_79B9_7F4A_7C15),
            }),
            snare: Slot::new(Snare {
                sample_rate,
                tune: 1.0,
                phases: [0.0; 2],
                tone: Shot::SILENT,
                rattle: Shot::SILENT,
                noise: WhiteNoise(0xD1B5_4A32_D192_ED03),
                highpass: StateVariableFilter::new(SvfMode::Highpass, 1_800.0, 0.7),
            }),
            hats: [
                Slot::new(Cymbal::new(CLOSED_HAT, 0x8CB9_2BA7_2F3D_8DD7)),
                Slot::new(Cymbal::new(OPEN_HAT, 0xABC9_8388_FB8F_AC03)),
            ],
            cymbals: [
                Slot::new(Cymbal::new(RIDE, 0x2545_F491_4F6C_DD1D)),
                Slot::new(Cymbal::new(CRASH, 0x6A09_E667_F3BC_C909)),
            ],
            choke_frames: frames_for(HAT_CHOKE_SECONDS, sample_rate),
            damp_frames: frames_for(DAMP_SECONDS, sample_rate),
        };
        kit.kick.sound.prepare(sample_rate);
        kit.snare.sound.prepare(sample_rate);
        for slot in kit.hats.iter_mut().chain(&mut kit.cymbals) {
            slot.sound.prepare(sample_rate);
        }
        params.visit(&mut |param| {
            kit.set_param(param.param_id(), param.plain());
        });
        kit
    }

    pub(crate) fn note_on(&mut self, note: u8, velocity: f32) {
        let Some(drum) = Drum::from_note(note) else {
            return;
        };
        let velocity = f64::from(velocity);
        let kit = self.settings;
        match drum {
            Drum::Kick => self.kick.strike(velocity, &kit),
            Drum::Snare => self.snare.strike(velocity, &kit),
            Drum::ClosedHat => {
                self.hats[1].fade_out(self.choke_frames);
                self.hats[0].strike(velocity, &kit);
            }
            Drum::OpenHat => self.hats[1].strike(velocity, &kit),
            Drum::Ride => self.cymbals[0].strike(velocity, &kit),
            Drum::Crash => self.cymbals[1].strike(velocity, &kit),
        }
    }

    /// Every drum stops at once.
    pub(crate) fn choke(&mut self) {
        self.kick.stop();
        self.snare.stop();
        for slot in self.hats.iter_mut().chain(&mut self.cymbals) {
            slot.stop();
        }
    }

    /// Every drum fades out over [`DAMP_SECONDS`].
    pub(crate) fn damp(&mut self) {
        let frames = self.damp_frames;
        self.kick.fade_out(frames);
        self.snare.fade_out(frames);
        for slot in self.hats.iter_mut().chain(&mut self.cymbals) {
            slot.fade_out(frames);
        }
    }

    pub(crate) fn active_voices(&self) -> usize {
        usize::from(self.kick.sounding)
            + usize::from(self.snare.sounding)
            + self
                .hats
                .iter()
                .chain(&self.cymbals)
                .filter(|slot| slot.sounding)
                .count()
    }

    /// Takes a new value; `false` if `id` is not a kit parameter.
    pub(crate) fn set_param(&mut self, id: ParamId, value: f64) -> bool {
        match id {
            params::KIT_TUNE => self.settings.tune = (value / 12.0 * LN_2).exp(),
            params::KIT_DECAY => self.settings.decay = value,
            _ => {
                let Some(index) = params::DRUM_LEVELS.iter().position(|&level| level == id)
                else {
                    return false;
                };
                let level = value.clamp(0.0, 1.0);
                match Drum::ALL[index] {
                    Drum::Kick => self.kick.level = level,
                    Drum::Snare => self.snare.level = level,
                    Drum::ClosedHat => self.hats[0].level = level,
                    Drum::OpenHat => self.hats[1].level = level,
                    Drum::Ride => self.cymbals[0].level = level,
                    Drum::Crash => self.cymbals[1].level = level,
                }
            }
        }
        true
    }

    /// Writes the kit's sound over `output`, adding the drums up in
    /// [`Drum::ALL`] order.
    pub(crate) fn render(&mut self, output: &mut [f32]) {
        if self.active_voices() == 0 {
            output.fill(0.0);
            return;
        }
        for sample in output {
            let mut sum = self.kick.next();
            sum += self.snare.next();
            sum += self.hats[0].next();
            sum += self.hats[1].next();
            sum += self.cymbals[0].next();
            sum += self.cymbals[1].next();
            *sample = sum as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_is_the_published_one() {
        let rows: Vec<(u8, char, &str)> = DRUM_MAP
            .iter()
            .map(|entry| (entry.note, entry.symbol, entry.name))
            .collect();
        assert_eq!(
            rows,
            [
                (36, 'b', "kick"),
                (38, 's', "snare"),
                (42, 'c', "closed_hat"),
                (46, 'o', "open_hat"),
                (49, 'x', "crash"),
                (51, 'r', "ride"),
            ]
        );
        for drum in Drum::ALL {
            assert_eq!(Drum::from_note(drum.note()), Some(drum));
            assert_eq!(Drum::from_symbol(drum.symbol()), Some(drum));
            assert_eq!(Drum::ALL[drum as usize], drum);
        }
        let notes: Vec<u8> = (0..=127).filter(|&n| Drum::from_note(n).is_some()).collect();
        assert_eq!(notes, [36, 38, 42, 46, 49, 51]);
        assert_eq!(Drum::from_symbol('q'), None);
    }

    #[test]
    fn the_map_json_is_exact() {
        assert_eq!(
            drum_map_json(),
            concat!(
                r#"[{"name":"kick","note":36,"symbol":"b"},"#,
                r#"{"name":"snare","note":38,"symbol":"s"},"#,
                r#"{"name":"closed_hat","note":42,"symbol":"c"},"#,
                r#"{"name":"open_hat","note":46,"symbol":"o"},"#,
                r#"{"name":"crash","note":49,"symbol":"x"},"#,
                r#"{"name":"ride","note":51,"symbol":"r"}]"#,
            )
        );
    }

    #[test]
    fn a_fade_falls_from_where_it_is_and_a_shorter_one_wins() {
        let mut fade = Fade::OFF;
        fade.start(4);
        let gains: Vec<(f64, bool)> = (0..4).map(|_| fade.next()).collect();
        assert_eq!(
            gains,
            [(1.0, false), (0.75, false), (0.5, false), (0.25, true)]
        );

        let mut fade = Fade::OFF;
        fade.start(10);
        fade.next();
        fade.next();
        fade.start(20); // longer than what is left: ignored
        assert_eq!(fade.remaining, 8);
        fade.start(2); // shorter: taken, from the current gain of 0.8
        assert!((fade.next().0 - 0.8).abs() < 1e-12);
        let (gain, ended) = fade.next();
        assert!((gain - 0.4).abs() < 1e-12 && ended);
    }

    #[test]
    fn white_noise_stays_in_range_and_is_centred() {
        let mut noise = WhiteNoise(1);
        let values: Vec<f64> = (0..100_000).map(|_| noise.next()).collect();
        assert!(values.iter().all(|v| (-1.0..1.0).contains(v)));
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        assert!(mean.abs() < 0.01, "mean {mean}");
    }
}
