//! The filter of a region, measured on notes: its responses, how keys,
//! velocity and the filter envelope move its cutoff, and its limits.

mod common;

use core::f64::consts::PI;

use common::*;
use manguebyte_dsp::{ProcessSpec, Processor};
use manguebyte_sampler::{Instrument, MemorySource, Sampler, WarningKind};

/// The pitch `*sine` sounds at its key center, which `pitch_keytrack=0`
/// keeps whatever the key.
fn middle_c() -> f64 {
    440.0 * 2f64.powf(-9.0 / 12.0)
}

/// The amplitude of the note's sine between `from` and `to` (frames), the
/// note struck on frame 0.
fn level(text: &str, key: u8, velocity: f32, from: usize, to: usize) -> f64 {
    let mut sampler = sampler_with(text, &mut MemorySource::new(), 8, 1);
    let output = render_mono(&mut sampler, to, &[(0, on(key, velocity))]);
    sine_fit(&output[from..to], middle_c(), RATE).0
}

/// The gain in decibels a region's opcodes give the sine of middle C, held
/// at its pitch, measured against the same region with none of them.
fn gain_db(opcodes: &str, key: u8, velocity: f32) -> f64 {
    let base = "<region> sample=*sine pitch_keytrack=0 amp_veltrack=0";
    let plain = level(base, key, velocity, 2_400, 12_000);
    let filtered = level(&format!("{base} {opcodes}"), key, velocity, 2_400, 12_000);
    20.0 * (filtered / plain).log10()
}

/// The digital Butterworth response the filter types are built to: the
/// analogue one at the prewarped frequency.
fn butterworth_db(poles: i32, high: bool, cutoff: f64, frequency: f64) -> f64 {
    let w = (PI * frequency / RATE).tan() / (PI * cutoff / RATE).tan();
    let w = if high { 1.0 / w } else { w };
    -10.0 * (1.0 + w.powi(2 * poles)).log10()
}

/// The cutoff a two-pole low-pass must have had to give `gain_db` at
/// `frequency`.
fn lowpass_cutoff(gain_db: f64, frequency: f64) -> f64 {
    let w = (10f64.powf(-gain_db / 10.0) - 1.0).powf(0.25);
    ((PI * frequency / RATE).tan() / w).atan() * RATE / PI
}

#[test]
fn a_two_pole_lowpass_is_three_decibels_down_at_its_cutoff() {
    let f = middle_c();
    let at_cutoff = gain_db(&format!("cutoff={f}"), 60, 1.0);
    assert!((at_cutoff + 3.0103).abs() < 0.02, "{at_cutoff} dB");
    let highpass = gain_db(&format!("fil_type=hpf_2p cutoff={f}"), 60, 1.0);
    assert!((highpass + 3.0103).abs() < 0.02, "{highpass} dB");
    // An octave above the cutoff: 12 dB per octave, as the prototype says.
    let octave = gain_db(&format!("cutoff={}", f / 2.0), 60, 1.0);
    let expected = butterworth_db(2, false, f / 2.0, f);
    assert!(
        (octave - expected).abs() < 0.02,
        "{octave} dB, not {expected}"
    );
}

#[test]
fn resonance_raises_the_gain_at_the_cutoff_by_its_decibels() {
    let f = middle_c();
    for resonance in [6.0, 12.0, 20.0] {
        let measured = gain_db(&format!("cutoff={f} resonance={resonance}"), 60, 1.0);
        assert!(
            (measured - (resonance - 3.0103)).abs() < 0.02,
            "{resonance} dB: {measured}"
        );
    }
    // The peak is near the cutoff: an octave below, a 12 dB resonance adds
    // far less than 12 dB.
    let below = gain_db(&format!("cutoff={} resonance=12", 2.0 * f), 60, 1.0);
    assert!(below > 0.0 && below < 3.0, "{below} dB");
}

#[test]
fn every_pole_count_falls_at_its_slope() {
    let f = middle_c();
    let cases = [
        ("lpf_1p", 1, false, f / 4.0),
        ("hpf_1p", 1, true, f * 4.0),
        ("lpf_2p", 2, false, f / 4.0),
        ("hpf_2p", 2, true, f * 4.0),
        ("lpf_4p", 4, false, f / 4.0),
        ("hpf_4p", 4, true, f * 4.0),
        ("lpf_6p", 6, false, f / 2.0),
        ("hpf_6p", 6, true, f * 2.0),
    ];
    for (name, poles, high, cutoff) in cases {
        let measured = gain_db(&format!("fil_type={name} cutoff={cutoff}"), 60, 1.0);
        let expected = butterworth_db(poles, high, cutoff, f);
        assert!(
            (measured - expected).abs() < 0.05,
            "{name}: {measured} dB, not {expected}"
        );
    }
    // Two octaves past the cutoff, four poles are about 48 dB down.
    let four = gain_db(&format!("fil_type=lpf_4p cutoff={}", f / 4.0), 60, 1.0);
    assert!((four + 48.0).abs() < 1.0, "{four} dB");
}

#[test]
fn band_types_pass_and_reject_around_their_cutoff() {
    let f = middle_c();
    let pass = gain_db(&format!("fil_type=bpf_2p cutoff={f}"), 60, 1.0);
    assert!(pass.abs() < 0.02, "{pass} dB");
    let off_centre = gain_db(&format!("fil_type=bpf_2p cutoff={}", 8.0 * f), 60, 1.0);
    assert!(off_centre < -15.0, "{off_centre} dB");
    let reject = gain_db(&format!("fil_type=brf_2p cutoff={f}"), 60, 1.0);
    assert!(reject < -40.0, "{reject} dB");
}

#[test]
fn keytrack_and_veltrack_move_the_cutoff_by_their_cents() {
    let f = middle_c();
    // The cutoff is middle C's frequency at the key center; the sine stays
    // there, so the gain tells where the cutoff went.
    let cases: [(&str, u8, f32, f64); 5] = [
        ("fil_keytrack=100", 72, 1.0, 1_200.0),
        ("fil_keytrack=100", 54, 1.0, -600.0),
        ("fil_keytrack=50 fil_keycenter=48", 60, 1.0, 600.0),
        ("fil_veltrack=-1200", 60, 1.0, -1_200.0),
        (
            "fil_veltrack=-1200",
            60,
            64.0 / 127.0,
            -1_200.0 * 64.0 / 127.0,
        ),
    ];
    for (opcodes, key, velocity, expected) in cases {
        let measured = gain_db(&format!("cutoff={f} {opcodes}"), key, velocity);
        let moved = cents(lowpass_cutoff(measured, f), f);
        assert!(
            (moved - expected).abs() < 0.5,
            "{opcodes} key {key}: {moved} cents, not {expected}"
        );
    }
}

#[test]
fn the_filter_envelope_shapes_the_cutoff_over_time() {
    let f = middle_c();
    let base = "<region> sample=*sine pitch_keytrack=0";
    let reference = level(base, 60, 1.0, 2_400, 12_000);
    // Two octaves of depth over a cutoff two octaves below the sine: the
    // envelope's full level puts the cutoff on the sine.
    let text = format!(
        "{base} cutoff={} fileg_depth=2400 fileg_attack=0.2 fileg_hold=0.2 fileg_sustain=50",
        f / 4.0
    );
    let mut sampler = sampler_with(&text, &mut MemorySource::new(), 8, 1);
    let output = render_mono(&mut sampler, 33_600, &[(0, on(60, 1.0))]);
    let gain_at = |from: f64, to: f64| {
        let window = &output[(from * RATE) as usize..(to * RATE) as usize];
        20.0 * (sine_fit(window, f, RATE).0 / reference).log10()
    };
    // Half way up the attack: half the depth, an octave below the sine.
    let attack = gain_at(0.095, 0.105);
    let octave = butterworth_db(2, false, f / 2.0, f);
    assert!(
        (attack - octave).abs() < 0.3,
        "attack: {attack} dB, not {octave}"
    );
    // Held at the top.
    let hold = gain_at(0.25, 0.39);
    assert!((hold + 3.0103).abs() < 0.05, "hold: {hold} dB");
    // At the sustain level, half the depth again.
    let sustain = gain_at(0.45, 0.7);
    assert!((sustain - octave).abs() < 0.05, "sustain: {sustain} dB");

    // `fileg_vel2depth` is depth at full velocity.
    let by_velocity = format!(
        "cutoff={} fileg_vel2depth=2400 fileg_hold=1 fileg_sustain=0",
        f / 4.0
    );
    let held = gain_db(&by_velocity, 60, 1.0);
    assert!((held + 3.0103).abs() < 0.05, "vel2depth: {held} dB");
}

#[test]
fn the_filter_envelope_releases_with_the_key() {
    let f = middle_c();
    // Sustained at the top, then released over 0.1 s: the cutoff falls
    // back to two octaves below the sine.
    let text = format!(
        "<region> sample=*sine pitch_keytrack=0 ampeg_release=100 cutoff={} \
         fileg_depth=2400 fileg_sustain=100 fileg_release=0.1",
        f / 4.0
    );
    let mut sampler = sampler_with(&text, &mut MemorySource::new(), 8, 1);
    let output = render_mono(&mut sampler, 24_000, &[(0, on(60, 1.0)), (4_800, off(60))]);
    let before = sine_fit(&output[2_400..4_800], f, RATE).0;
    let after = sine_fit(&output[19_200..24_000], f, RATE).0;
    // The amplitude envelope loses about 0.4 dB in this time; the
    // filter takes the rest.
    let fall = 20.0 * (after / before).log10();
    let expected = butterworth_db(2, false, f / 4.0, f) + 3.0103;
    assert!(
        (fall - expected).abs() < 1.0,
        "{fall} dB, not about {expected}"
    );
}

#[test]
fn a_region_without_a_cutoff_or_with_a_type_not_played_is_unfiltered() {
    let base = "<region> sample=*sine pitch_keytrack=0";
    let mut plain = sampler_with(base, &mut MemorySource::new(), 8, 1);
    let plain = render_mono(&mut plain, 4_800, &[(0, on(60, 1.0))]);
    for opcodes in ["resonance=20 fil_type=hpf_4p", "fil_type=pkf_2p cutoff=100"] {
        let text = format!("{base} {opcodes}");
        let instrument = Instrument::from_sfz(&text, &mut MemorySource::new()).unwrap();
        let refused = instrument.warnings().iter().any(
            |w| matches!(&w.kind, WarningKind::UnknownOpcode { name, .. } if &**name == "fil_type"),
        );
        assert_eq!(refused, opcodes.contains("pkf"), "{opcodes}");
        let mut sampler = sampler_with(&text, &mut MemorySource::new(), 8, 1);
        let output = render_mono(&mut sampler, 4_800, &[(0, on(60, 1.0))]);
        assert_eq!(output, plain, "{opcodes}");
    }
}

#[test]
fn the_cutoff_is_held_below_nyquist_at_every_sample_rate() {
    let at_rate = |text: &str, rate: f64, key: u8| {
        let instrument = Instrument::from_sfz(text, &mut MemorySource::new()).unwrap();
        let (mut sampler, _loader) = Sampler::<f32>::with_instrument(instrument, 8);
        sampler.prepare(&ProcessSpec {
            sample_rate: rate,
            max_block_size: 512,
            num_channels: 1,
        });
        render_mono(&mut sampler, (0.1 * rate) as usize, &[(0, on(key, 1.0))])
    };
    for rate in [22_050.0, 44_100.0, 96_000.0] {
        let base = "<region> sample=*sine pitch_keytrack=0";
        let plain = at_rate(base, rate, 60);
        let high = at_rate(&format!("{base} cutoff=40000"), rate, 60);
        let half = plain.len() / 2;
        let plain = sine_fit(&plain[half..], middle_c(), rate).0;
        let high = sine_fit(&high[half..], middle_c(), rate).0;
        // Held to 0.49 of the rate, the cutoff is far above the sine.
        assert!(
            (high / plain - 1.0).abs() < 1e-3,
            "{rate}: {high} against {plain}"
        );

        // A sweep across the whole range at full resonance stays bounded.
        let sweep = "<region> sample=*sine cutoff=20 resonance=40 fil_type=lpf_4p \
                     fileg_depth=12000 fileg_attack=0.02 fileg_decay=0.02 fileg_sustain=0";
        let output = at_rate(sweep, rate, 96);
        assert!(
            output.iter().all(|s| s.is_finite() && s.abs() < 1_000.0),
            "{rate}"
        );
    }
}
