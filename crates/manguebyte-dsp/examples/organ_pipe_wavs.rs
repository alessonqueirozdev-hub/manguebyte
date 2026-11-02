//! Writes an organ pipe and a small chord as WAV files, plain and voiced, to
//! compare by ear.
//!
//! ```text
//! cargo run --release -p manguebyte-dsp --example organ_pipe_wavs -- <folder>
//! ```
//!
//! The folder must be named; it is made if it is not there. It gets four
//! files of 16-bit stereo at 48 kHz:
//!
//! - `principal_c4_plain.wav` and `principal_c4_voiced.wav`: one pipe of a
//!   principal at 8', middle C, for four seconds.
//! - `chord_plain.wav` and `chord_voiced.wav`: C4, E4, G4 and C5 together,
//!   four pipes of the same rank.
//!
//! Both versions of a sound have the same harmonics, the same slow drift on
//! each partial and the same release, and go through the same fixed scale, so
//! the difference is the voicing alone. The plain one is what a pipe
//! rendered as a clean bank of partials sounds like: its partials rise
//! together, by a gain ramp of 30 ms. The voiced one has
//! [`PipeVoicing::FLUE_PRINCIPAL`]'s air, attack and jitter. They are dry, with
//! no room, which a real listener would have and which hides a good deal.
//! The harmonics' levels are a judgement of a principal's, a rough octave
//! slope and not a measured pipe, and so is the drift's size.

use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

use manguebyte_dsp::{Partial, PartialBank, PipeVoicing};

const RATE: f64 = 48_000.0;
const RATE_U32: u32 = 48_000;
const CONTROL: usize = 64;
const PARTIALS: usize = 24;
/// Everything is scaled by this, so the versions compare at their levels.
const SCALE: f32 = 0.3;

/// A fixed generator (xorshift64*), so the two versions drift alike.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn bipolar(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (x >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// A principal's harmonics: falling about 5.5 dB an octave, the even ones
/// a little under the odd ones up to the eighth. A judgement.
fn level(harmonic: usize) -> f32 {
    let octaves = (harmonic as f32).log2();
    let dip = if harmonic % 2 == 0 && harmonic <= 8 {
        -1.5
    } else {
        0.0
    };
    10f32.powf((-5.5 * octaves + dip) / 20.0)
}

/// One pipe, rendering into stereo.
struct Pipe {
    bank: PartialBank,
    frequency: f64,
    drift: Vec<f64>,
    rng: Rng,
    released: bool,
}

impl Pipe {
    /// A pipe at `frequency` hertz, at `pan`. Number `pipe` picks its noise
    /// and its drift.
    fn new(pipe: u64, frequency: f64, pan: f32, voiced: bool) -> Self {
        let voicing = PipeVoicing::FLUE_PRINCIPAL;
        let mut bank = PartialBank::new(PARTIALS);
        bank.set_sample_rate(RATE);
        bank.set_voicing_seed(pipe + 100);
        let count = (0..PARTIALS)
            .take_while(|&i| frequency * (i + 1) as f64 <= 0.45 * RATE)
            .count();
        bank.set_len(count);
        for i in 0..count {
            let harmonic = (i + 1) as f32;
            let hz = frequency * f64::from(harmonic);
            bank.set(
                i,
                Partial {
                    freq_hz: hz,
                    gain: 0.0,
                    pan,
                },
            );
            bank.set_phase(i, (i as f64 * 0.381_966).fract());
            if voiced {
                bank.set_gain(i, level(i + 1));
                bank.set_air(i, voicing.air(harmonic, hz));
                bank.start_attack(i, &voicing.attack(harmonic, RATE));
            } else {
                bank.ramp_to(i, level(i + 1), (0.030 * RATE) as usize);
            }
        }
        if voiced {
            bank.set_wind_jitter(voicing.jitter);
        }
        Self {
            bank,
            frequency,
            drift: vec![0.0; count],
            rng: Rng::new(pipe + 1),
            released: false,
        }
    }

    /// A control block's slow drift, the same in both versions: white noise
    /// of 5.5 cents at most through a one-pole low-pass of 150 ms, as
    /// Caecilia's partials have.
    fn control(&mut self) {
        let coefficient = 1.0 - (-(CONTROL as f64) / (0.15 * RATE)).exp();
        for i in 0..self.bank.len() {
            let drift = &mut self.drift[i];
            *drift += coefficient * (self.rng.bipolar() * 5.5 - *drift);
            let hz = self.frequency * (i + 1) as f64 * (*drift / 1_200.0).exp2();
            self.bank.set_frequency(i, hz);
        }
    }

    fn release(&mut self) {
        self.released = true;
        for i in 0..self.bank.len() {
            // The upper partials go first.
            let frames = (0.150 * RATE) as usize / (1 + i / 4);
            self.bank.ramp_to(i, 0.0, frames);
        }
    }
}

/// Plays `pipes` together for `seconds`, releasing them for the last 200 ms.
fn play(mut pipes: Vec<Pipe>, seconds: f64) -> (Vec<f32>, Vec<f32>) {
    let frames = (seconds * RATE) as usize / CONTROL * CONTROL;
    let release_at = frames - (0.2 * RATE) as usize;
    let (mut left, mut right) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    for at in (0..frames).step_by(CONTROL) {
        for pipe in &mut pipes {
            pipe.control();
            if at >= release_at && !pipe.released {
                pipe.release();
            }
            pipe.bank
                .render_add(&mut left[at..at + CONTROL], &mut right[at..at + CONTROL]);
        }
    }
    for sample in left.iter_mut().chain(right.iter_mut()) {
        *sample *= SCALE;
    }
    (left, right)
}

fn write_wav(folder: &Path, name: &str, left: &[f32], right: &[f32]) -> io::Result<()> {
    let data_bytes = u32::try_from(left.len() * 4).expect("a short file");
    let mut bytes = Vec::with_capacity(44 + left.len() * 4);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes()); // format chunk size
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&2u16.to_le_bytes()); // two channels
    bytes.extend_from_slice(&RATE_U32.to_le_bytes());
    bytes.extend_from_slice(&(RATE_U32 * 4).to_le_bytes()); // bytes a second
    bytes.extend_from_slice(&4u16.to_le_bytes()); // bytes a frame
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits a sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    let (mut peak, mut squares) = (0.0f32, 0.0f64);
    for (&l, &r) in left.iter().zip(right) {
        for sample in [l, r] {
            peak = peak.max(sample.abs());
            squares += f64::from(sample) * f64::from(sample);
            let value = (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16;
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    fs::write(folder.join(name), bytes)?;
    let rms = (squares / (2 * left.len().max(1)) as f64).sqrt();
    println!(
        "{name:28} peak {:6.1} dBFS, rms {:6.1} dBFS",
        20.0 * peak.log10(),
        20.0 * rms.log10()
    );
    Ok(())
}

fn run(folder: &Path) -> io::Result<()> {
    fs::create_dir_all(folder)?;
    const C4: f64 = 261.625_565;
    for (name, voiced) in [("plain", false), ("voiced", true)] {
        let (left, right) = play(vec![Pipe::new(0, C4, 0.0, voiced)], 4.0);
        write_wav(folder, &format!("principal_c4_{name}.wav"), &left, &right)?;

        let chord = [
            (0, 0.0f64, -0.3f32),
            (1, 4.0, 0.2),
            (2, 7.0, -0.1),
            (3, 12.0, 0.3),
        ]
        .iter()
        .map(|&(pipe, semitones, pan)| {
            let frequency = C4 * (semitones / 12.0).exp2();
            Pipe::new(pipe, frequency, pan, voiced)
        })
        .collect();
        let (left, right) = play(chord, 4.0);
        let (left, right): (Vec<f32>, Vec<f32>) = left
            .iter()
            .zip(&right)
            .map(|(l, r)| (l * 0.4, r * 0.4))
            .unzip();
        write_wav(folder, &format!("chord_{name}.wav"), &left, &right)?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let Some(folder) = env::args_os().nth(1) else {
        eprintln!("usage: organ_pipe_wavs <folder>");
        return ExitCode::FAILURE;
    };
    if let Err(error) = run(Path::new(&folder)) {
        eprintln!("could not write the files: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
