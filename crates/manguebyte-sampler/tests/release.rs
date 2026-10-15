//! Release triggers, the sustain pedal, `first` and `legato`.

mod common;

use common::*;
use manguebyte_dsp::Processor;
use manguebyte_sampler::MemorySource;

const ATTACK_HZ: f64 = 400.0;
const RELEASE_HZ: f64 = 1_000.0;

/// An attack region and two release regions, a soft and a loud one, told
/// apart by level as well as pitch.
fn source() -> MemorySource {
    MemorySource::new()
        .with("attack.wav", sine_wav(ATTACK_HZ, 48_000, 1.0, 0.5))
        .with("release.wav", sine_wav(RELEASE_HZ, 48_000, 0.1, 0.5))
        .with("dc.wav", constant_wav(0.25, 4_800, 48_000))
}

const TEXT: &str = "<global> amp_veltrack=0
    <region> sample=attack.wav ampeg_release=0
    <region> sample=release.wav trigger=release hivel=63 volume=-6.0206
    <region> sample=release.wav trigger=release lovel=64";

/// The first frame from which `signal` is not silent.
fn onset(signal: &[f32]) -> Option<usize> {
    signal.iter().position(|&s| s != 0.0)
}

#[test]
fn a_release_region_starts_on_the_frame_the_key_comes_up() {
    let mut source = source();
    let mut sampler = sampler_with(TEXT, &mut source, 8, 1);
    let output = render_mono(&mut sampler, 9_600, &[(0, on(60, 1.0)), (2_400, off(60))]);
    let held = &output[100..2_400];
    assert!(amplitude_at(held, ATTACK_HZ, RATE) > 0.45);
    assert!(amplitude_at(held, RELEASE_HZ, RATE) < 1e-3);
    // The attack region stops at once (no release time); the release
    // region starts on frame 2400 itself: a sine starts at 0, so its
    // first non-zero frame is 2401.
    assert_eq!(onset(&output[2_400..]), Some(1));
    let released = &output[2_500..7_100];
    assert!(amplitude_at(released, ATTACK_HZ, RATE) < 1e-3);
    let level = amplitude_at(released, RELEASE_HZ, RATE);
    assert!((level - 0.5).abs() < 0.01, "{level}");
    // It plays its 0.1 s through and ends.
    assert!(output[2_400 + 4_800..].iter().all(|&s| s == 0.0));
    assert_eq!(sampler.active_voices(), 0);
}

#[test]
fn a_release_region_uses_the_velocity_of_the_note_on() {
    let mut source = source();
    let mut sampler = sampler_with(TEXT, &mut source, 8, 1);
    // Struck softly, released hard: the soft release layer (-6 dB) plays.
    let events = [
        (0, on(60, 0.3)),
        (
            1_000,
            manguebyte_dsp::event::EventKind::NoteOff {
                note: note(60),
                velocity: 1.0,
            },
        ),
    ];
    let output = render_mono(&mut sampler, 6_000, &events);
    let level = amplitude_at(&output[1_100..5_700], RELEASE_HZ, RATE);
    assert!((level - 0.25).abs() < 0.005, "{level}");
}

#[test]
fn the_pedal_holds_release_regions_until_it_comes_up_and_release_key_does_not_wait() {
    let text = "<global> amp_veltrack=0
        <region> sample=attack.wav ampeg_release=0
        <region> sample=release.wav trigger=release
        <region> sample=dc.wav trigger=release_key";
    let mut source = source();
    let mut sampler = sampler_with(text, &mut source, 8, 1);
    let events = [
        (0, on(60, 1.0)),
        (100, controller(64, 127)),
        (1_000, off(60)),
        (3_000, controller(64, 0)),
    ];
    let output = render_mono(&mut sampler, 9_600, &events);
    // Held by the pedal, the attack region goes on past the key's release,
    // and the release_key region (a constant 0.25) joins it on frame 1000.
    let before = &output[1_100..2_900];
    assert!(amplitude_at(before, ATTACK_HZ, RATE) > 0.45);
    assert!(amplitude_at(before, RELEASE_HZ, RATE) < 1e-3);
    let mean: f64 = before.iter().map(|&s| f64::from(s)).sum::<f64>() / before.len() as f64;
    assert!((mean - 0.25).abs() < 0.01, "{mean}");
    let dc_start = output[999] - output[998];
    assert!(dc_start.abs() < 0.1);
    assert!(
        (output[1_000] - output[999] - 0.25).abs() < 0.1,
        "{}",
        output[1_000] - output[999]
    );
    // The pedal comes up on frame 3000: the attack region stops, the
    // release region starts.
    let after = &output[3_100..5_800];
    assert!(amplitude_at(after, ATTACK_HZ, RATE) < 1e-3);
    assert!((amplitude_at(after, RELEASE_HZ, RATE) - 0.5).abs() < 0.01);
}

#[test]
fn rt_decay_lowers_a_release_region_by_the_time_held() {
    let text = "<global> amp_veltrack=0
        <region> sample=attack.wav
        <region> sample=release.wav trigger=release rt_decay=6";
    let mut source = source();
    let level_after = |held: usize, source: &mut MemorySource| {
        let mut sampler = sampler_with(text, source, 8, 1);
        let output = render_mono(
            &mut sampler,
            held + 6_000,
            &[(0, on(60, 1.0)), (held, off(60))],
        );
        amplitude_at(&output[held + 100..held + 4_700], RELEASE_HZ, RATE)
    };
    let quick = level_after(48, &mut source);
    let one_second = level_after(48_048, &mut source);
    let drop = 20.0 * (one_second / quick).log10();
    assert!((drop + 6.0).abs() < 0.05, "{drop} dB");
}

#[test]
fn first_plays_only_alone_and_legato_only_over_another_note() {
    let text = "<global> amp_veltrack=0 ampeg_release=0
        <region> sample=attack.wav trigger=first
        <region> sample=release.wav trigger=legato";
    let mut source = source();
    let mut sampler = sampler_with(text, &mut source, 8, 1);
    let events = [(0, on(60, 1.0)), (4_800, on(62, 1.0))];
    let output = render_mono(&mut sampler, 9_600, &events);
    let alone = &output[100..4_700];
    assert!(amplitude_at(alone, ATTACK_HZ, RATE) > 0.45);
    assert!(amplitude_at(alone, RELEASE_HZ, RATE) < 1e-3);
    // The second note, over the first, plays only the legato region (the
    // first note's region goes on). The legato region is a whole tone up.
    let legato_hz = RELEASE_HZ * 2f64.powf(2.0 / 12.0);
    let together = &output[4_900..9_500];
    assert!(amplitude_at(together, ATTACK_HZ, RATE) > 0.45);
    assert!(amplitude_at(together, legato_hz, RATE) > 0.45);
    assert!(amplitude_at(together, ATTACK_HZ * 2f64.powf(2.0 / 12.0), RATE) < 0.01);
    sampler.reset();
    assert_eq!(sampler.active_voices(), 0);
}

#[test]
fn each_note_on_a_key_releases_with_its_own_velocity() {
    // Two overlapping notes on one key, told apart by identifier. The soft
    // one is released first: its release region is the soft layer; then
    // the loud one, with the loud layer.
    use manguebyte_dsp::event::{EventKind, NoteRef};
    let mut source = MemorySource::new()
        .with("soft.wav", sine_wav(700.0, 48_000, 0.1, 0.5))
        .with("loud.wav", sine_wav(1_300.0, 48_000, 0.1, 0.5));
    let text = "<global> amp_veltrack=0 pitch_keytrack=0
        <region> sample=soft.wav trigger=release hivel=63
        <region> sample=loud.wav trigger=release lovel=64";
    let mut sampler = sampler_with(text, &mut source, 8, 1);
    let soft = NoteRef::with_id(0, 60, 1);
    let loud = NoteRef::with_id(0, 60, 2);
    let off = |note| EventKind::NoteOff {
        note,
        velocity: 0.0,
    };
    let events = [
        (
            0,
            EventKind::NoteOn {
                note: soft,
                velocity: 0.3,
            },
        ),
        (
            1_000,
            EventKind::NoteOn {
                note: loud,
                velocity: 1.0,
            },
        ),
        (2_000, off(soft)),
        (8_000, off(loud)),
    ];
    let output = render_mono(&mut sampler, 14_000, &events);
    let first = &output[2_100..6_700];
    let soft_level = amplitude_at(first, 700.0, RATE);
    let loud_level = amplitude_at(first, 1_300.0, RATE);
    assert!(
        soft_level > 0.45 && loud_level < 0.01,
        "first release: soft {soft_level}, loud {loud_level}"
    );
    let second = &output[8_100..12_700];
    let soft_level = amplitude_at(second, 700.0, RATE);
    let loud_level = amplitude_at(second, 1_300.0, RATE);
    assert!(
        loud_level > 0.45 && soft_level < 0.01,
        "second release: soft {soft_level}, loud {loud_level}"
    );
    assert_eq!(sampler.active_voices(), 0);
}

#[test]
fn rt_decay_counts_from_the_note_on_of_the_note_released() {
    // The note struck first and held longest is released while a later
    // one on the same key is still down: its release region loses 6 dB
    // for its own second held, not for the later note's.
    use manguebyte_dsp::event::{EventKind, NoteRef};
    let text = "<global> amp_veltrack=0
        <region> sample=attack.wav
        <region> sample=release.wav trigger=release rt_decay=6";
    let mut source = source();
    let early = NoteRef::with_id(0, 60, 7);
    let late = NoteRef::with_id(0, 60, 8);
    let events = [
        (
            0,
            EventKind::NoteOn {
                note: early,
                velocity: 1.0,
            },
        ),
        (
            47_952,
            EventKind::NoteOn {
                note: late,
                velocity: 1.0,
            },
        ),
        (
            48_000,
            EventKind::NoteOff {
                note: early,
                velocity: 0.0,
            },
        ),
    ];
    let mut sampler = sampler_with(text, &mut source, 8, 1);
    let output = render_mono(&mut sampler, 54_000, &events);
    let level = amplitude_at(&output[48_100..52_700], RELEASE_HZ, RATE);
    let drop = 20.0 * (level / 0.5).log10();
    assert!((drop + 6.0).abs() < 0.05, "{drop} dB");
}

#[test]
fn notes_without_identifiers_release_the_latest_on_their_key() {
    // MIDI-style notes carry no identifier: two note-ons on one key and two
    // note-offs end the latest first, each with its own velocity.
    let mut source = MemorySource::new()
        .with("soft.wav", sine_wav(700.0, 48_000, 0.1, 0.5))
        .with("loud.wav", sine_wav(1_300.0, 48_000, 0.1, 0.5));
    let text = "<global> amp_veltrack=0 pitch_keytrack=0
        <region> sample=soft.wav trigger=release hivel=63
        <region> sample=loud.wav trigger=release lovel=64";
    let mut sampler = sampler_with(text, &mut source, 8, 1);
    let events = [
        (0, on(60, 0.3)),
        (1_000, on(60, 1.0)),
        (2_000, off(60)),
        (8_000, off(60)),
    ];
    let output = render_mono(&mut sampler, 14_000, &events);
    assert!(amplitude_at(&output[2_100..6_700], 1_300.0, RATE) > 0.45);
    assert!(amplitude_at(&output[2_100..6_700], 700.0, RATE) < 0.01);
    assert!(amplitude_at(&output[8_100..12_700], 700.0, RATE) > 0.45);
    assert!(amplitude_at(&output[8_100..12_700], 1_300.0, RATE) < 0.01);
}
