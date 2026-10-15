//! Mangue Pluck's output stays in range with every setting at its maximum,
//! without flat tops, and its level is what it says (TECH_DEBT 43).
//!
//! Before, sixteen voices were simply added up and the level went up to
//! +6 dB, so with everything at its maximum the output reached +27 dBFS on
//! the REAPER project of TECH_DEBT 42 and about +35 dBFS on a chord. Now
//! each voice has a share of the headroom, the resonance and the reverb
//! give back what they add, and a soft knee bends every sample louder than
//! [`OUTPUT_KNEE_DB`] smoothly towards [`OUTPUT_CEILING_DB`], which no
//! finite sample reaches; a limiter with no look-ahead (so no latency)
//! after it only stands guard. The level is a plain gain ahead of the knee,
//! and up to -6 dB one note at full velocity never reaches it at the
//! settings measured. Without the knee, the REAPER project at every maximum
//! (resonance included) reaches +1.8 dBFS and the chords below +21.7 dBFS.
//!
//! An earlier version had only the limiter, which with no look-ahead set
//! each rising sample over the ceiling to exactly the ceiling: 336 samples
//! of the chords and 321 of the project came out flat at the ceiling, in
//! runs of up to five, a hard clip.

use mangue_pluck::{OUTPUT_CEILING_DB, OUTPUT_KNEE_DB, Pluck, PluckParams};
use manguebyte::prelude::*;

const RATE: f64 = 48_000.0;

fn ceiling() -> f32 {
    decibels::db_to_gain(OUTPUT_CEILING_DB) as f32
}

/// Every parameter at its maximum. (Its minimum, for none of them, makes
/// anything louder.)
fn maximum(params: &PluckParams) {
    params.cutoff.set(12_000.0);
    params.resonance.set(8.0);
    params.decay.set(5.0);
    params.release.set(5.0);
    params.reverb.set(1.0);
    params.level.set(6.0);
}

fn assert_in_range(sound: &AudioBuffer<f32>, what: &str) {
    for channel in 0..sound.num_channels() {
        let bad = sound.channel(channel).iter().position(|s| !s.is_finite());
        assert_eq!(bad, None, "{what}: channel {channel}");
    }
    // The limiter's gain is worked out in f64 and applied in f32, which
    // may round up by half a unit.
    let limit = ceiling() * (1.0 + f32::EPSILON);
    assert!(
        sound.peak() <= limit,
        "{what}: peak {} ({:+.2} dBFS)",
        sound.peak(),
        20.0 * sound.peak().log10()
    );
}

#[test]
fn chords_at_every_maximum_stay_under_the_ceiling() {
    let mut harness = Harness::<Pluck>::new(RATE, 512);
    maximum(harness.params());
    // Sixteen notes struck on one frame, so their sawtooths start in step
    // and add up, twice, then the same with every note's volume expression
    // at CLAP's maximum (four times) and the filters opened by pressure.
    for (round, start) in [0_u64, 96_000, 192_000].into_iter().enumerate() {
        for index in 0..16_u8 {
            let key = 36 + index * 4;
            harness.note(start, 60_000, key, 1.0);
            if round == 2 {
                let note = NoteRef::new(0, key);
                for (expression, value) in [(Expression::Volume, 4.0), (Expression::Pressure, 1.0)]
                {
                    harness.schedule(
                        start,
                        EventKind::NoteExpression {
                            note,
                            expression,
                            value,
                        },
                    );
                }
            }
        }
    }
    let sound = harness.render(300_000);
    assert_in_range(&sound, "three chords");
    // The knee brings the peaks close to the ceiling rather than far below
    // it.
    assert!(sound.peak() > 0.95 * ceiling(), "peak {}", sound.peak());
}

#[test]
fn the_reaper_project_at_every_maximum_stays_under_the_ceiling() {
    // The material of TECH_DEBT 42, with the resonance at its maximum too.
    let mut harness = Harness::<Pluck>::new(RATE, 512);
    maximum(harness.params());
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut below = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    for index in 0..37_u64 {
        let start = index * 1_200_000 / 38 + below(12_000);
        let length = 24_000 + below(264_000);
        harness.note(start, length, 36 + below(61) as u8, 1.0);
    }
    let sound = harness.render(1_392_000);
    assert_in_range(&sound, "the REAPER project");
}

/// One note at full velocity, over the settings that make it loud, with
/// the level at -6 dB and at -12 dB: the second is the first times
/// 10^(-6/20), to the rounding of that one multiplication, so neither the
/// knee nor the limiter touched the first. The loudest settings are a low cutoff that the
/// resonance sweeps across the first harmonics, a long decay, and the
/// reverb fully in. The release only shapes the tail, and is left at its
/// default.
#[test]
fn up_to_minus_6_db_one_note_is_never_shaped() {
    let ratio = decibels::db_to_gain(-6.0);
    let (louder_db, quieter_db) = (-6.0, -12.0);
    let mut loudest = (0.0_f32, String::new());
    for cutoff in [150.0, 400.0, 1_800.0, 12_000.0] {
        for resonance in [0.5, 8.0] {
            for decay in [0.3, 5.0] {
                for reverb in [0.0, 1.0] {
                    let render = |level: f32, key: u8| {
                        let mut harness = Harness::<Pluck>::new(RATE, 512);
                        let params = harness.params();
                        params.cutoff.set(cutoff);
                        params.resonance.set(resonance);
                        params.decay.set(decay);
                        params.reverb.set(reverb);
                        params.level.set(level);
                        harness.note(0, 24_000, key, 1.0);
                        harness.render(36_000)
                    };
                    for key in [36, 66, 96] {
                        let place = format!(
                            "cutoff {cutoff}, resonance {resonance}, decay {decay}, reverb {reverb}, key {key}"
                        );
                        let plain = render(louder_db, key);
                        let quieter = render(quieter_db, key);
                        if plain.peak() > loudest.0 {
                            loudest = (plain.peak(), place.clone());
                        }
                        for channel in 0..2 {
                            let pairs = plain.channel(channel).iter().zip(quieter.channel(channel));
                            for (frame, (&a, &b)) in pairs.enumerate() {
                                // Each is the same sound times its gain, so
                                // they agree to the rounding of the two
                                // gains and the two products: four halves
                                // of a unit in the last place.
                                let expected = f64::from(a) * ratio;
                                let error = (f64::from(b) - expected).abs();
                                assert!(
                                    error <= 3e-7 * expected.abs(),
                                    "{place}, frame {frame}: {b} against {a} x {ratio}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    let (peak, place) = loudest;
    assert!(peak < knee(), "{peak} with {place}");
    println!(
        "the loudest single note at -6 dB peaks at {peak} ({:+.2} dBFS), with {place}",
        20.0 * peak.log10()
    );
}

#[test]
fn the_limiter_adds_no_latency() {
    for rate in [44_100.0, 48_000.0, 96_000.0] {
        let mut harness = Harness::<Pluck>::new(rate, 256);
        assert_eq!(harness.plugin_mut().latency_samples(), 0, "{rate}");
        harness.params().reverb.set(0.0);
        harness.note(0, 10_000, 69, 1.0);
        let sound = harness.render(64);
        let first = sound.channel(0).iter().position(|&s| s != 0.0);
        // The sawtooth starts at its zero crossing, so the first sound is
        // on the frame after the note's.
        assert_eq!(first, Some(1), "{rate}");
    }
}

/// At the default settings, one note at full velocity peaks between -19
/// and -22 dBFS, depending on the key, with room for chords: sixteen notes
/// struck one by one, a sixteenth note apart, still stay under the ceiling.
#[test]
fn the_default_sound_leaves_room_for_chords() {
    for key in [36, 60, 84] {
        let mut harness = Harness::<Pluck>::new(RATE, 512);
        harness.note(0, 24_000, key, 1.0);
        let peak = 20.0 * harness.render(48_000).peak().log10();
        println!("key {key} at the default settings peaks at {peak:.1} dBFS");
        assert!((-22.5..-18.5).contains(&peak), "key {key}: {peak:.1} dBFS");
    }
    let mut harness = Harness::<Pluck>::new(RATE, 512);
    for index in 0..16_u8 {
        harness.note(u64::from(index) * 6_000, 96_000, 40 + index * 3, 1.0);
    }
    let peak = harness.render(200_000).peak();
    assert!(peak < 0.98 * ceiling(), "{peak}");
}

fn knee() -> f32 {
    decibels::db_to_gain(OUTPUT_KNEE_DB) as f32
}

/// How many samples sit at the ceiling, to within four units in the last
/// place, and the longest run of them: what a clip leaves behind.
fn flat_tops(sound: &AudioBuffer<f32>) -> (usize, usize) {
    let ceiling = ceiling();
    let at_ceiling = |s: f32| (s.abs() - ceiling).abs() <= ceiling * 4.0 * f32::EPSILON;
    let mut longest = 0;
    let mut total = 0;
    for channel in 0..sound.num_channels() {
        let mut run = 0;
        for &sample in sound.channel(channel) {
            if at_ceiling(sample) {
                run += 1;
                total += 1;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
    }
    (total, longest)
}

/// How many samples went into the knee, out of all of them.
fn past_the_knee(sound: &AudioBuffer<f32>) -> (usize, usize) {
    let knee = knee();
    let mut count = 0;
    for channel in 0..sound.num_channels() {
        count += sound
            .channel(channel)
            .iter()
            .filter(|s| s.abs() > knee)
            .count();
    }
    (count, sound.num_channels() * sound.num_frames())
}

/// Sixteen notes struck on one frame at every maximum, which add up to
/// +21.7 dBFS before the knee: nothing comes out flat at the ceiling, and
/// every sample stays under it.
#[test]
fn a_chord_at_every_maximum_is_not_flat_topped() {
    let mut harness = Harness::<Pluck>::new(RATE, 512);
    maximum(harness.params());
    for index in 0..16_u8 {
        harness.note(0, 60_000, 36 + index * 4, 1.0);
    }
    let sound = harness.render(96_000);
    let (total, longest) = flat_tops(&sound);
    let (bent, all) = past_the_knee(&sound);
    println!(
        "chord: peak {:+.3} dBFS, {bent} of {all} samples in the knee, {total} at the ceiling",
        20.0 * sound.peak().log10()
    );
    assert_eq!((total, longest), (0, 0));
    assert!(sound.peak() < ceiling(), "peak {}", sound.peak());
    assert!(bent > 0, "the material does not reach the knee");
}

/// The REAPER project's material at every maximum, the case TECH_DEBT 43
/// is about.
#[test]
fn the_reaper_project_at_every_maximum_is_not_flat_topped() {
    let mut harness = Harness::<Pluck>::new(RATE, 512);
    maximum(harness.params());
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut below = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n
    };
    for index in 0..37_u64 {
        let start = index * 1_200_000 / 38 + below(12_000);
        let length = 24_000 + below(264_000);
        harness.note(start, length, 36 + below(61) as u8, 1.0);
    }
    let sound = harness.render(1_392_000);
    let (total, longest) = flat_tops(&sound);
    let (bent, all) = past_the_knee(&sound);
    println!(
        "project: peak {:+.3} dBFS, {bent} of {all} samples in the knee, {total} at the ceiling",
        20.0 * sound.peak().log10()
    );
    assert_eq!((total, longest), (0, 0));
    assert!(sound.peak() < ceiling(), "peak {}", sound.peak());
}
