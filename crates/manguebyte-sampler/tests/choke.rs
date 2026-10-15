//! Choke groups, group polyphony and voice stealing.

mod common;

use common::*;
use manguebyte_dsp::event::EventKind;
use manguebyte_sampler::MemorySource;

const OPEN_HZ: f64 = 700.0;
const CLOSED_HZ: f64 = 1_300.0;

fn source() -> MemorySource {
    MemorySource::new()
        .with("open.wav", sine_wav(OPEN_HZ, 48_000, 1.0, 0.5))
        .with("closed.wav", sine_wav(CLOSED_HZ, 48_000, 1.0, 0.5))
        .with("dc.wav", constant_wav(0.5, 48_000, 48_000))
}

/// Open hat on 46 (group 2, silenced by group 1), closed hat on 42
/// (group 1), both one-shots.
const HATS: &str = "<global> amp_veltrack=0 loop_mode=one_shot pitch_keytrack=0
    <region> sample=open.wav key=46 group=2 off_by=1
    <region> sample=closed.wav key=42 group=1";

#[test]
fn the_closed_hat_silences_the_open_hat_within_six_milliseconds() {
    let mut source = source();
    let mut sampler = sampler_with(HATS, &mut source, 8, 1);
    let output = render_mono(
        &mut sampler,
        9_600,
        &[(0, on(46, 1.0)), (4_800, on(42, 1.0))],
    );
    assert!(amplitude_at(&output[100..4_700], OPEN_HZ, RATE) > 0.45);
    // The fade takes 6 ms, 288 frames, and is a straight line from where
    // the open hat was.
    let after = &output[4_800 + 288..];
    assert!(amplitude_at(after, OPEN_HZ, RATE) < 1e-4);
    assert!(amplitude_at(after, CLOSED_HZ, RATE) > 0.45);
    assert_eq!(sampler.active_voices(), 1);
}

#[test]
fn without_off_by_the_two_overlap() {
    let mut source = source();
    let text = HATS.replace(" off_by=1", "");
    let mut sampler = sampler_with(&text, &mut source, 8, 1);
    let output = render_mono(
        &mut sampler,
        9_600,
        &[(0, on(46, 1.0)), (4_800, on(42, 1.0))],
    );
    let after = &output[5_100..];
    assert!(amplitude_at(after, OPEN_HZ, RATE) > 0.45);
    assert!(amplitude_at(after, CLOSED_HZ, RATE) > 0.45);
}

#[test]
fn off_modes_fade_by_their_own_rules() {
    // A constant sample shows the fade's shape directly.
    let run = |mode: &str| {
        let text = format!(
            "<global> amp_veltrack=0 pitch_keytrack=0
             <region> sample=dc.wav key=46 group=2 off_by=1 {mode} ampeg_release=0.1
             <region> sample=*silence key=42 group=1"
        );
        let mut source = source();
        let mut sampler = sampler_with(&text, &mut source, 8, 1);
        render_mono(
            &mut sampler,
            14_400,
            &[(0, on(46, 1.0)), (4_800, on(42, 1.0))],
        )
    };
    let fast = run("off_mode=fast");
    // 288 frames from 0.5 down to nothing, in a straight line.
    assert!((fast[4_800] - 0.5).abs() < 1e-6);
    assert!((fast[4_800 + 144] - 0.25).abs() < 0.01);
    assert!(fast[4_800 + 288..].iter().all(|&s| s == 0.0));
    let time = run("off_mode=time off_time=0.05");
    assert!((time[4_800 + 1_200] - 0.25).abs() < 0.01);
    assert!(time[4_800 + 2_400..].iter().all(|&s| s == 0.0));
    // `normal` releases: 90 dB in 0.1 s, so 45 dB at 2400 frames.
    let normal = run("off_mode=normal");
    let level = 20.0 * (f64::from(normal[4_800 + 2_399]) / 0.5).log10();
    assert!((level + 45.0).abs() < 0.1, "{level}");
    assert!(normal[4_800 + 4_800..].iter().all(|&s| s == 0.0));
}

#[test]
fn regions_started_by_the_same_note_do_not_choke_each_other() {
    let mut source = source();
    let text = "<global> amp_veltrack=0 pitch_keytrack=0
        <region> sample=open.wav key=50 group=1 off_by=1
        <region> sample=closed.wav key=50 group=1 off_by=1";
    let mut sampler = sampler_with(text, &mut source, 8, 1);
    let output = render_mono(
        &mut sampler,
        9_600,
        &[(0, on(50, 1.0)), (4_800, on(50, 1.0))],
    );
    let first = &output[100..4_700];
    assert!(amplitude_at(first, OPEN_HZ, RATE) > 0.45);
    assert!(amplitude_at(first, CLOSED_HZ, RATE) > 0.45);
    // The second note chokes the first one's two voices, not its own.
    assert_eq!(sampler.active_voices(), 2);
}

#[test]
fn a_group_polyphony_limit_fades_the_oldest_of_the_group() {
    let mut source = source();
    let text = "<global> amp_veltrack=0 ampeg_release=1
        <region> sample=dc.wav group=3 polyphony=2";
    let mut sampler = sampler_with(text, &mut source, 16, 1);
    let events = [
        (0, on(60, 1.0)),
        (100, on(62, 1.0)),
        (200, on(64, 1.0)),
        (300, on(65, 1.0)),
    ];
    let output = render_mono(&mut sampler, 2_000, &events);
    // Each note adds 0.5; at most two sound once the fades are over.
    assert!((output[150] - 1.0).abs() < 1e-6);
    assert!((output[1_000] - 1.0).abs() < 1e-6);
    assert_eq!(sampler.active_voices(), 2);
}

#[test]
fn a_full_sampler_steals_the_voice_missed_least() {
    let mut source = source();
    let text = "<global> amp_veltrack=0 ampeg_release=1
        <region> sample=dc.wav lokey=0 hikey=127";
    let mut sampler = sampler_with(text, &mut source, 2, 1);
    assert_eq!(sampler.polyphony(), 2);
    // 60 and 62 held; 62 released; then 64: the releasing 62 goes, not 60.
    let events = [
        (0, on(60, 1.0)),
        (0, on(62, 1.0)),
        (100, off(62)),
        (200, on(64, 1.0)),
    ];
    let output = render_mono(&mut sampler, 1_000, &events);
    assert!((output[50] - 1.0).abs() < 1e-6);
    // After the 6 ms fade, 60 and 64 remain, both at full level.
    assert!((output[600] - 1.0).abs() < 1e-6, "{}", output[600]);
    assert_eq!(sampler.active_voices(), 2);
    // With every voice held, the oldest is taken.
    let output = render_mono(&mut sampler, 1_000, &[(0, on(65, 1.0))]);
    assert!((output[500] - 1.0).abs() < 1e-6);
    assert_eq!(sampler.active_voices(), 2);
    // Many notes at once never exceed the voices allocated, spares
    // included, and never stop sounding abruptly mid-block.
    let burst: Vec<(usize, EventKind)> = (0..40).map(|i| (i * 3, on(20 + i as u8, 1.0))).collect();
    render_mono(&mut sampler, 1_000, &burst);
    assert!(sampler.active_voices() <= 4);
}

#[test]
fn a_regions_polyphony_does_not_cut_unrelated_regions() {
    // Without a `group` opcode, a region's own polyphony counts only its
    // own voices: key 62 is not part of key 60's limit.
    let mut source = MemorySource::new()
        .with("low.wav", constant_wav(0.25, 48_000, 48_000))
        .with("high.wav", constant_wav(0.5, 48_000, 48_000));
    let text = "<global> amp_veltrack=0 ampeg_release=1
        <region> sample=low.wav key=60 polyphony=1
        <region> sample=high.wav key=62";
    let mut sampler = sampler_with(text, &mut source, 16, 1);
    let events = [(0, on(62, 1.0)), (100, on(60, 1.0)), (1_200, on(60, 1.0))];
    let output = render_mono(&mut sampler, 2_400, &events);
    // Both keys held: 0.5 + 0.25.
    assert!(
        (output[1_000] - 0.75).abs() < 1e-6,
        "key 62 was cut by key 60's polyphony=1: {}",
        output[1_000]
    );
    // Key 60 again: its first voice gives way, 62 stays.
    assert!((output[2_000] - 0.75).abs() < 1e-6, "{}", output[2_000]);
    assert_eq!(sampler.active_voices(), 2);
}

#[test]
fn a_polyphony_set_in_a_header_counts_the_regions_under_it() {
    // A <group> header's polyphony=2 covers its two regions together, and
    // not the region of the next group.
    let mut source = MemorySource::new()
        .with("a.wav", constant_wav(0.125, 48_000, 48_000))
        .with("b.wav", constant_wav(0.25, 48_000, 48_000));
    let text = "<global> amp_veltrack=0 ampeg_release=1
        <group> polyphony=2
        <region> sample=a.wav key=60
        <region> sample=a.wav key=61
        <group>
        <region> sample=b.wav key=62";
    let mut sampler = sampler_with(text, &mut source, 16, 1);
    let events = [
        (0, on(62, 1.0)),
        (0, on(62, 1.0)),
        (100, on(60, 1.0)),
        (200, on(61, 1.0)),
        (300, on(60, 1.0)),
    ];
    let output = render_mono(&mut sampler, 1_500, &events);
    // Two of 62 (0.5) and two of the first group (0.25), once the oldest of
    // the group, the first 60, has faded.
    assert!((output[1_000] - 0.75).abs() < 1e-6, "{}", output[1_000]);
    assert_eq!(sampler.active_voices(), 4);
}

#[test]
fn polyphony_counts_voices_not_notes() {
    // Three layers per note and four voices: the second note's three
    // layers need two of the first note's voices, and take those, never
    // one another's.
    let mut source = MemorySource::new()
        .with("a.wav", constant_wav(0.125, 48_000, 48_000))
        .with("b.wav", constant_wav(0.25, 48_000, 48_000))
        .with("c.wav", constant_wav(0.5, 48_000, 48_000));
    let text = "<global> amp_veltrack=0 ampeg_release=1
        <region> sample=a.wav
        <region> sample=b.wav
        <region> sample=c.wav";
    let mut sampler = sampler_with(text, &mut source, 4, 1);
    assert_eq!(sampler.polyphony(), 4);
    let output = render_mono(&mut sampler, 2_000, &[(0, on(60, 1.0)), (500, on(64, 1.0))]);
    assert!((output[400] - 0.875).abs() < 1e-6, "{}", output[400]);
    // After the fades: every layer of the new note (0.875) and the one
    // layer of the old note left, its loudest (0.5).
    assert!((output[1_500] - 1.375).abs() < 1e-6, "{}", output[1_500]);
    assert_eq!(sampler.active_voices(), 4);
}
