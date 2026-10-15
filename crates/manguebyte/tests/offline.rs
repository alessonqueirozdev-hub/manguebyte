//! Offline rendering is deterministic and does not depend on block size.
//!
//! A host may hand a plugin blocks of any length, and a different length on
//! every call. The sound must not depend on it: notes, automation and effect
//! tails have to land on the same frames however the timeline is cut up.
//! This renders the example song several ways and requires the results to be
//! identical, bit for bit, and does the same with denser material: more
//! notes at once than the synth has voices, rendered again after a reset
//! and played again once it has ended.

#[path = "../examples/common/mod.rs"]
mod common;

use std::sync::Arc;

use common::RiffPlayer;
use manguebyte::prelude::*;

const SAMPLE_RATE: f64 = 48_000.0;

/// Renders the whole song, asking for blocks of the lengths in `pattern`,
/// repeated for as long as it takes.
fn render(max_block_size: usize, pattern: &[usize]) -> AudioBuffer<f32> {
    let mut player = RiffPlayer::new(SAMPLE_RATE, 2, max_block_size);
    let frames = player.total_frames();
    let mut song = AudioBuffer::<f32>::new(2, frames);
    let mut position = 0;
    for &length in pattern.iter().cycle() {
        if position == frames {
            break;
        }
        let length = length.min(frames - position);
        song.windowed(position..position + length, |block| player.render(block));
        position += length;
    }
    assert!(player.is_finished());
    song
}

#[test]
fn the_song_is_the_same_whatever_the_block_size() {
    let reference = render(256, &[256]);
    assert!(reference.peak() > 0.1, "the reference render is silent");

    for (max_block_size, pattern) in [
        (64, &[64][..]),
        (1024, &[1024]),
        // Odd lengths that never line up with a note boundary.
        (997, &[997]),
        // A host that changes its mind on every call.
        (512, &[1, 511, 7, 300, 64, 2, 129]),
        // Blocks larger than the player's own, which it must split itself.
        (48, &[5_000]),
    ] {
        let other = render(max_block_size, pattern);
        assert!(
            other == reference,
            "blocks of {pattern:?} (max {max_block_size}) changed the output"
        );
    }
}

#[test]
fn two_renders_are_identical() {
    assert!(render(256, &[256]) == render(256, &[256]));
}

#[test]
fn the_song_contains_the_notes_that_were_asked_for() {
    use manguebyte::dsp::fft::{Complex, RealFft};
    use manguebyte::dsp::window::{Symmetry, Window};

    let song = render(256, &[256]);

    // The first half second holds an A minor chord over a low A.
    let len = 16_384;
    let mut slice: Vec<f32> = song.channel(0)[2_000..2_000 + len].to_vec();
    Window::Hann.apply(&mut slice, Symmetry::Periodic);
    let mut fft = RealFft::<f32>::new(len);
    let mut spectrum = vec![Complex::ZERO; fft.spectrum_len()];
    fft.forward(&slice, &mut spectrum);

    let level_at = |hz: f64| {
        let bin = (hz * len as f64 / SAMPLE_RATE).round() as usize;
        (bin - 1..=bin + 1)
            .map(|index| spectrum[index].norm())
            .fold(0.0f32, f32::max)
    };
    // Something no note in the song produces: between A2 and its octave.
    let floor = level_at(160.0);
    for key in [45.0, 57.0, 60.0, 64.0] {
        let level = level_at(note_to_hz(key));
        assert!(
            level > 8.0 * floor,
            "key {key}: level {level} against a floor of {floor}"
        );
    }
}

// --- Dense material (TECH_DEBT 42) ----------------------------------------
//
// In REAPER, two offline renders of one project with many overlapping notes
// differed by a unit in the last place. The song above has few voices at a
// time; this material has more notes sounding than there are voices, all
// at full velocity, with every voice's samples far from whole numbers, so
// that the order in which the synth adds its voices up shows in the last
// bit of the output.

/// One note of an instrument like the example `plugins/pluck`: a sawtooth
/// through a resonant low-pass that closes as the note fades.
struct Saw {
    oscillator: Oscillator,
    filter: StateVariableFilter<f32>,
    envelope: Adsr,
    velocity: f32,
}

impl Saw {
    fn new() -> Self {
        Self {
            oscillator: Oscillator::new(Waveform::Saw, 220.0),
            filter: StateVariableFilter::new(SvfMode::Lowpass, 9_000.0, 0.9),
            envelope: Adsr::new(AdsrParams {
                attack: 0.003,
                decay: 1.5,
                sustain: 0.4,
                release: 1.2,
            }),
            velocity: 0.0,
        }
    }
}

impl Voice for Saw {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.oscillator.set_sample_rate(spec.sample_rate);
        self.envelope.set_sample_rate(spec.sample_rate);
        self.filter.prepare(&ProcessSpec {
            num_channels: 1,
            ..*spec
        });
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        self.oscillator
            .set_frequency(note_to_hz(f64::from(note.key)));
        self.velocity = velocity;
        self.envelope.note_on();
    }

    fn note_off(&mut self, _velocity: f32) {
        self.envelope.note_off();
    }

    fn choke(&mut self) {
        self.envelope.silence();
        self.filter.reset();
        self.oscillator.reset_phase();
    }

    fn is_active(&self) -> bool {
        self.envelope.is_active()
    }

    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        for frame in 0..buffer.num_frames() {
            let level = self.envelope.next_sample();
            self.filter.set_frequency(9_000.0 * (0.25 + 0.75 * level));
            let raw = self.oscillator.next_sample() as f32;
            let sample = self.filter.process_sample(0, raw) * level as f32 * self.velocity;
            for channel in buffer.iter_channels_mut() {
                channel[frame] += sample;
            }
        }
    }
}

struct DenseParams {
    reverb: FloatParam,
}
param_set!(DenseParams { reverb });

/// Sixteen of those voices and a reverb, as a plugin, so that the harness
/// can play it as a host does.
struct Dense {
    params: Arc<DenseParams>,
    synth: Synth<Saw>,
    reverb: Reverb<f32>,
}

impl Plugin for Dense {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.dense",
        name: "Dense",
        vendor: "Manguebyte",
        version: "1",
        description: "Many voices at once",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    type Params = DenseParams;

    fn new() -> Self {
        Self {
            params: Arc::new(DenseParams {
                reverb: FloatParam::new("reverb", "Reverb", FloatRange::linear(0.0, 1.0), 0.5),
            }),
            synth: Synth::with_voices(16, Saw::new),
            reverb: Reverb::new().with_decay_seconds(2.0),
        }
    }

    fn params(&self) -> Arc<DenseParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, spec: &ProcessSpec) {
        self.synth.prepare(spec);
        self.reverb.prepare(spec);
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.synth.process_with(buffer, context);
        // Held still in these tests, so reading it once per block is exact.
        self.reverb.set_mix(f64::from(self.params.reverb.value()));
        self.reverb.process(buffer);
    }

    fn reset(&mut self) {
        self.synth.reset();
        self.reverb.reset();
    }
}

/// Seconds of dense material; renders run on for the release.
const DENSE_SECONDS: f64 = 10.0;

fn dense_frames() -> usize {
    ((DENSE_SECONDS + 1.5) * SAMPLE_RATE) as usize
}

/// 37 notes at full velocity in ten seconds, keys from 36 to 96, chords
/// struck on one frame and notes up to three seconds long: often more than
/// sixteen at once, so voices are stolen too.
fn schedule_dense(harness: &mut Harness<Dense>, from: u64) {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut below = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    let span = (DENSE_SECONDS * SAMPLE_RATE) as u64;
    let mut start = 0;
    for index in 0..37_u64 {
        if index % 4 != 3 {
            start = index * span / 38 + below(4_800);
        }
        let length = (12_000 + below(132_000)).min(span - start);
        harness.note(from + start, length, 36 + below(61) as u8, 1.0);
    }
}

fn render_dense(max_block_size: usize, pattern: &[usize]) -> AudioBuffer<f32> {
    let mut harness =
        Harness::<Dense>::new(SAMPLE_RATE, max_block_size).with_block_pattern(pattern);
    schedule_dense(&mut harness, 0);
    harness.render(dense_frames())
}

/// Where two renders part, if they do, and by how much against the
/// loudest sample, in decibels.
fn parting(a: &AudioBuffer<f32>, b: &AudioBuffer<f32>) -> Option<String> {
    let mut first = None;
    let mut largest = 0.0_f32;
    for channel in 0..a.num_channels() {
        for (frame, (x, y)) in a
            .channel(channel)
            .iter()
            .zip(b.channel(channel))
            .enumerate()
        {
            if x.to_bits() != y.to_bits() {
                first = Some(first.map_or(frame, |at: usize| at.min(frame)));
                largest = largest.max((x - y).abs());
            }
        }
    }
    first.map(|frame| {
        let below = 20.0 * (largest / a.peak().max(b.peak())).log10();
        format!("from frame {frame}, by up to {largest:e}, {below:.0} dB")
    })
}

#[test]
fn dense_material_is_the_same_whatever_the_block_size() {
    let reference = render_dense(256, &[256]);
    assert!(reference.peak() > 1.0, "too few notes overlap");
    for (max_block_size, pattern) in [
        (64, &[64][..]),
        (997, &[997]),
        (512, &[1, 511, 7, 300, 64, 2, 129]),
    ] {
        let other = render_dense(max_block_size, pattern);
        assert_eq!(parting(&other, &reference), None, "blocks of {pattern:?}");
    }
}

#[test]
fn dense_material_after_a_reset_is_the_same_as_the_first_time() {
    let first = render_dense(256, &[256]);
    let mut harness = Harness::<Dense>::new(SAMPLE_RATE, 256).with_block_pattern(&[256]);
    schedule_dense(&mut harness, 0);
    harness.render(dense_frames());
    harness.reset();
    let from = harness.position();
    schedule_dense(&mut harness, from);
    let again = harness.render(dense_frames());
    assert_eq!(parting(&again, &first), None);
}

#[test]
fn dense_material_played_again_after_it_ends_is_the_same_without_a_reset() {
    // No reverb, whose tail would rightly carry over: the synth alone.
    let mut harness = Harness::<Dense>::new(SAMPLE_RATE, 256);
    harness.params().reverb.set(0.0);
    schedule_dense(&mut harness, 0);
    let first = harness.render(dense_frames());
    assert!(first.peak() > 1.0);
    // Every note has ended: the material ends with a second and a half of
    // room for the release, which is 1.2 s.
    let quiet = harness.render(4_800);
    assert_eq!(quiet.peak(), 0.0);
    let from = harness.position();
    schedule_dense(&mut harness, from);
    let again = harness.render(dense_frames());
    assert_eq!(parting(&again, &first), None);
}
