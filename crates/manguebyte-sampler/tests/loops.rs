//! Loops: continuity at the loop point, the modes, offset, end and the
//! crossfade.

mod common;

use common::*;
use manguebyte_dsp::Processor;
use manguebyte_sampler::{Interpolation, MemorySource};

/// 480 Hz at 48 kHz: exactly 100 frames per cycle, so a loop of a whole
/// number of hundreds of frames joins without a seam.
fn source() -> MemorySource {
    MemorySource::new()
        .with("tone.wav", sine_wav(480.0, 48_000, 1.0, 0.5))
        .with("dc.wav", constant_wav(0.5, 1_000, 48_000))
}

/// The ideal output of the looped tone: the sine at the voice's position.
fn ideal(frame: usize, step: f64) -> f64 {
    0.5 * (core::f64::consts::TAU * 480.0 * frame as f64 * step / 48_000.0).sin()
}

#[test]
fn a_seamless_loop_plays_through_its_loop_point_with_no_discontinuity() {
    for (key, interpolation, tolerance) in [
        (60, Interpolation::Sinc, 1e-6),
        (67, Interpolation::Sinc, 1e-4),
        (53, Interpolation::Sinc, 1e-4),
        (67, Interpolation::Cubic, 2e-3),
    ] {
        let mut source = source();
        // Loop frames 1000 to 1999: ten cycles.
        let text =
            "<region> sample=tone.wav loop_mode=loop_continuous loop_start=1000 loop_end=1999";
        let mut sampler = sampler_with(text, &mut source, 4, 1);
        sampler.set_interpolation(interpolation);
        let output = render_mono(&mut sampler, 96_000, &[(0, on(key, 1.0))]);
        let step = 2f64.powf((f64::from(key) - 60.0) / 12.0);
        // Two seconds hold dozens of passes through the loop point; every
        // frame is the sine it should be.
        let mut worst: f64 = 0.0;
        for (frame, &sample) in output.iter().enumerate().skip(100) {
            worst = worst.max((f64::from(sample) - ideal(frame, step)).abs());
        }
        assert!(
            worst < tolerance,
            "key {key} {interpolation:?}: off by {worst}"
        );
        // And the largest change between neighbouring frames is that of
        // the sine itself, never a jump.
        let slope = output
            .windows(2)
            .skip(100)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0f32, f32::max);
        let sine_slope = 0.5 * core::f64::consts::TAU * 480.0 * step / 48_000.0;
        assert!(
            f64::from(slope) <= sine_slope * 1.001 + tolerance,
            "{slope} vs {sine_slope}"
        );
    }
}

#[test]
fn the_continuity_check_finds_a_seam_when_there_is_one() {
    // The same check on a loop that ends a quarter cycle off: the jump is
    // seen. This shows the test above could fail.
    let mut source = source();
    let text = "<region> sample=tone.wav loop_mode=loop_continuous loop_start=1000 loop_end=2024";
    let mut sampler = sampler_with(text, &mut source, 4, 1);
    let output = render_mono(&mut sampler, 9_600, &[(0, on(60, 1.0))]);
    let slope = output
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0f32, f32::max);
    let sine_slope = 0.5 * core::f64::consts::TAU * 480.0 / 48_000.0;
    assert!(f64::from(slope) > 5.0 * sine_slope, "{slope}");

    // A crossfade over the loop's last 300 frames hides the same seam.
    let text = "<region> sample=tone.wav loop_mode=loop_continuous loop_start=1000 loop_end=2024 \
                loop_crossfade=0.00625";
    let mut sampler = sampler_with(text, &mut source, 4, 1);
    let output = render_mono(&mut sampler, 9_600, &[(0, on(60, 1.0))]);
    let slope = output
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0f32, f32::max);
    assert!(f64::from(slope) < 1.5 * sine_slope, "{slope}");
}

#[test]
fn a_crossfaded_loop_of_a_seamless_tone_stays_the_tone() {
    // Crossfading a loop that already joins mixes the sine with itself in
    // phase, so nothing changes.
    let mut source = source();
    let text = "<region> sample=tone.wav loop_start=1000 loop_end=1999 loop_crossfade=0.005";
    let mut sampler = sampler_with(text, &mut source, 4, 1);
    let output = render_mono(&mut sampler, 48_000, &[(0, on(60, 1.0))]);
    for (frame, &sample) in output.iter().enumerate() {
        assert!(
            (f64::from(sample) - ideal(frame, 1.0)).abs() < 1e-6,
            "frame {frame}"
        );
    }
}

/// Frames of `signal` that are not silent.
fn sounding(signal: &[f32]) -> usize {
    signal.iter().filter(|&&s| s != 0.0).count()
}

#[test]
fn offset_and_end_bound_the_frames_played() {
    let mut source = source();
    let text = "<region> sample=dc.wav offset=100 end=399";
    let mut sampler = sampler_with(text, &mut source, 4, 1);
    let output = render_mono(&mut sampler, 2_000, &[(10, on(60, 1.0))]);
    // Frames 100 to 399 of the sample, both included: 300 frames from 10.
    assert_eq!(sounding(&output), 300);
    assert!(output[10..310].iter().all(|&s| (s - 0.5).abs() < 1e-6));
    assert_eq!(sampler.active_voices(), 0);
}

#[test]
fn loop_modes_differ_in_what_the_release_does() {
    let text = |mode: &str| {
        format!("<region> sample=tone.wav {mode} loop_start=1000 loop_end=1999 ampeg_release=0.5")
    };
    let run = |mode: &str, frames: usize| {
        let mut source = source();
        let mut sampler = sampler_with(&text(mode), &mut source, 4, 1);
        let output = render_mono(&mut sampler, frames, &[(0, on(60, 1.0)), (2_500, off(60))]);
        (output, sampler.active_voices())
    };

    // no_loop: plays past the loop to the end of its second, or stops when
    // the release ends (0.5 s), whichever is first.
    let (output, _) = run("loop_mode=no_loop", 72_000);
    assert_eq!(sounding(&output[26_500..]), 0);
    // one_shot ignores the release: the whole second plays at full level.
    let (output, voices) = run("loop_mode=one_shot", 50_000);
    assert!((rms(&output[40_000..47_900]) - 0.5 / 2f64.sqrt()).abs() < 1e-3);
    assert_eq!(sounding(&output[48_000..]), 0);
    assert_eq!(voices, 0);
    // loop_continuous goes on looping through the release, until the
    // release has fallen 90 dB.
    let (output, voices) = run("loop_mode=loop_continuous", 72_000);
    assert!(sounding(&output[26_000..26_400]) > 0);
    assert_eq!(sounding(&output[26_600..]), 0);
    assert_eq!(voices, 0);
    // loop_sustain leaves the loop at the release and runs on through the
    // sample; with a long release it reaches the end of the file.
    let text = "<region> sample=tone.wav loop_mode=loop_sustain loop_start=1000 loop_end=1999 \
                ampeg_release=10";
    let mut source = source();
    let mut sampler = sampler_with(text, &mut source, 4, 1);
    let events = [(0, on(60, 1.0)), (20_000, off(60))];
    let output = render_mono(&mut sampler, 72_000, &events);
    // Until the release the voice loops; frame 20000 is 19 whole passes
    // of the 1000-frame loop after frame 1000, so the voice is back at the
    // loop start. From there it plays on to the end of the file, frame
    // 47999, and stops: 47000 frames.
    let position_at_release: usize = 1_000;
    let remaining = 48_000 - position_at_release;
    assert_eq!(sounding(&output[20_000..]), remaining);
    assert_eq!(sampler.active_voices(), 0);
    sampler.reset();
}

#[test]
fn a_wav_smpl_loop_is_used_when_the_text_gives_none() {
    // Build the tone with a smpl chunk looping frames 1000 to 1999.
    let mut bytes = sine_wav(480.0, 48_000, 0.1, 0.5);
    let mut chunk = Vec::new();
    for value in [0u32, 0, 20_833, 60, 0, 0, 0, 1, 0, 0, 0, 1_000, 1_999, 0, 0] {
        chunk.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(b"smpl");
    bytes.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&chunk);
    let riff = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&riff.to_le_bytes());
    let mut source = MemorySource::new().with("looped.wav", bytes);
    let mut sampler = sampler_with("<region> sample=looped.wav", &mut source, 4, 1);
    // The file is 4800 frames long; looped, it sounds for as long as held.
    let output = render_mono(&mut sampler, 24_000, &[(0, on(60, 1.0))]);
    for (frame, &sample) in output.iter().enumerate() {
        assert!(
            (f64::from(sample) - ideal(frame, 1.0)).abs() < 1e-6,
            "frame {frame}"
        );
    }
}

#[test]
fn once_inside_the_loop_only_the_loop_is_heard() {
    // Frames 0 to 999 hold another tone and frames 2000 on hold noise, so a
    // voice that read past either end of the loop would be heard doing it.
    let mut frames = sine(1_234.0, RATE, 1_000, 0.5);
    frames.extend(sine(480.0, RATE, 1_000, 0.5));
    let mut random = Random::new(3);
    frames.extend((0..2_000).map(|_| (random.unit() - 0.5) as f32));
    let mut source = MemorySource::new().with("mixed.wav", wav_bytes(&[frames], 48_000));
    for (key, interpolation, tolerance) in [
        (67, Interpolation::Sinc, 1e-4),
        (55, Interpolation::Sinc, 1e-4),
        (67, Interpolation::Cubic, 2e-3),
    ] {
        let text =
            "<region> sample=mixed.wav loop_mode=loop_continuous loop_start=1000 loop_end=1999";
        let mut sampler = sampler_with(text, &mut source, 4, 1);
        sampler.set_interpolation(interpolation);
        let output = render_mono(&mut sampler, 48_000, &[(0, on(key, 1.0))]);
        let step = 2f64.powf((f64::from(key) - 60.0) / 12.0);
        // From the frame the voice is a kernel's width inside the loop.
        let inside = (1_100.0 / step).ceil() as usize;
        let worst = output
            .iter()
            .enumerate()
            .skip(inside)
            .map(|(frame, &s)| (f64::from(s) - ideal(frame, step)).abs())
            .fold(0.0, f64::max);
        assert!(
            worst < tolerance,
            "key {key} {interpolation:?}: off by {worst}"
        );
    }
}

#[test]
fn releasing_a_sustain_loop_inside_its_crossfade_does_not_click() {
    // The loop 1000..2024 ends a quarter cycle off; a 300-frame crossfade
    // (1725..2024) hides the seam while looping. The key comes up in the
    // middle of the crossfade; the long release keeps the level, so any
    // jump is the sample reader's.
    for (key, interpolation, release_at) in [
        (60, Interpolation::Sinc, 1_875),
        (60, Interpolation::Sinc, 2_020),
        (60, Interpolation::Cubic, 1_800),
        (67, Interpolation::Sinc, 1_200),
        (67, Interpolation::Cubic, 1_200),
        (55, Interpolation::Sinc, 2_400),
    ] {
        let mut source = source();
        let text = "<region> sample=tone.wav loop_mode=loop_sustain loop_start=1000 \
                    loop_end=2024 loop_crossfade=0.00625 ampeg_release=10";
        let mut sampler = sampler_with(text, &mut source, 4, 1);
        sampler.set_interpolation(interpolation);
        let output = render_mono(
            &mut sampler,
            12_000,
            &[(0, on(key, 1.0)), (release_at, off(key))],
        );
        let slope = output[100..]
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0f32, f32::max);
        let step = 2f64.powf((f64::from(key) - 60.0) / 12.0);
        let sine_slope = 0.5 * core::f64::consts::TAU * 480.0 * step / 48_000.0;
        assert!(
            f64::from(slope) < 1.5 * sine_slope,
            "key {key} {interpolation:?}, released on {release_at}: jump of {slope} \
             against a slope of {sine_slope}"
        );
        // After the release the voice plays on through the file.
        assert!(sounding(&output[11_000..]) > 900, "key {key}");
    }
}

#[test]
fn frames_outside_offset_and_end_are_not_heard() {
    // The same 480 Hz tone, alone in one file and between bursts of noise
    // in another; offset and end select it from the second. Played off
    // pitch, both must sound the same, looped or not.
    let tone = sine(480.0, RATE, 2_000, 0.5);
    let mut random = Random::new(11);
    let mut noise = || -> Vec<f32> {
        (0..1_000)
            .map(|_| (2.0 * random.unit() - 1.0) as f32)
            .collect()
    };
    let mut framed = noise();
    framed.extend_from_slice(&tone);
    framed.extend(noise());
    let mut source = MemorySource::new()
        .with("tone.wav", wav_bytes(std::slice::from_ref(&tone), 48_000))
        .with("framed.wav", wav_bytes(&[framed], 48_000));
    for (key, interpolation) in [
        (61, Interpolation::Sinc),
        (55, Interpolation::Sinc),
        (72, Interpolation::Sinc),
        (61, Interpolation::Cubic),
    ] {
        let mut play = |text: &str| {
            let mut sampler = sampler_with(text, &mut source, 4, 1);
            sampler.set_interpolation(interpolation);
            render_mono(&mut sampler, 4_800, &[(0, on(key, 1.0))])
        };
        let alone = play("<region> sample=tone.wav ampeg_release=0");
        let selected = play("<region> sample=framed.wav offset=1000 end=2999 ampeg_release=0");
        assert_eq!(alone, selected, "key {key} {interpolation:?}");
        // A sustain loop inside the selection, released: the same again.
        let alone = play(
            "<region> sample=tone.wav loop_mode=loop_continuous loop_start=500 loop_end=1499",
        );
        let selected = play(
            "<region> sample=framed.wav offset=1000 end=2999 loop_mode=loop_continuous \
             loop_start=1500 loop_end=2499",
        );
        assert_eq!(alone, selected, "looped, key {key} {interpolation:?}");
    }
}
