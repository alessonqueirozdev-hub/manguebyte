//! The right regions play for each key, velocity and channel.

mod common;

use common::*;
use manguebyte_dsp::Processor;
use manguebyte_dsp::event::{EventKind, NoteRef};
use manguebyte_sampler::MemorySource;

/// Each region plays a sine at its own frequency, whatever the key
/// (`pitch_keytrack=0`), so the output says which regions sounded.
const FREQUENCIES: [f64; 6] = [300.0, 500.0, 700.0, 900.0, 1_100.0, 1_300.0];

fn source() -> MemorySource {
    let mut source = MemorySource::new();
    for (index, &frequency) in FREQUENCIES.iter().enumerate() {
        source.insert(
            &format!("r{index}.wav"),
            sine_wav(frequency, 48_000, 0.5, 0.5),
        );
    }
    source
}

const TEXT: &str = "
<global> pitch_keytrack=0 pitch_keycenter=60 amp_veltrack=0
<group> lokey=0 hikey=47
<region> sample=r0.wav hivel=63           // soft layer of the low keys
<region> sample=r1.wav lovel=64           // loud layer of the low keys
<group> lokey=c3 hikey=b4
<region> sample=r2.wav                    // 48 to 71, every velocity
<region> sample=r3.wav lokey=60 hikey=83 lovel=100   // layered on top, 60 to 83, loud only
<group>
<region> sample=r4.wav lokey=84 hikey=127 lochan=2 hichan=2 // channel 2 only
<region> sample=r5.wav key=a#7
";

/// Which regions the table above says a note plays.
fn expected(channel: u8, key: u8, velocity: u8) -> Vec<usize> {
    let mut regions = Vec::new();
    if key <= 47 {
        regions.push(if velocity <= 63 { 0 } else { 1 });
    }
    if (48..=71).contains(&key) {
        regions.push(2);
    }
    if (60..=83).contains(&key) && velocity >= 100 {
        regions.push(3);
    }
    if key >= 84 && channel == 1 {
        regions.push(4);
    }
    if key == 106 {
        regions.push(5);
    }
    regions
}

/// Which regions sounded, read from the output's spectrum.
fn heard(signal: &[f32]) -> Vec<usize> {
    let mut regions = Vec::new();
    for (index, &frequency) in FREQUENCIES.iter().enumerate() {
        let amplitude = amplitude_at(signal, frequency, RATE);
        assert!(
            !(0.01..=0.2).contains(&amplitude),
            "ambiguous level {amplitude} at {frequency} Hz"
        );
        if amplitude > 0.2 {
            regions.push(index);
        }
    }
    regions
}

#[test]
fn each_key_velocity_and_channel_plays_its_regions() {
    let mut source = source();
    let mut sampler = sampler_with(TEXT, &mut source, 16, 1);
    let mut checked = 0;
    for channel in [0u8, 1] {
        for key in (0..128u8)
            .step_by(3)
            .chain([47, 48, 59, 60, 71, 72, 83, 84, 106, 127])
        {
            for velocity in [1u8, 40, 63, 64, 99, 100, 127] {
                sampler.reset();
                let kind = EventKind::NoteOn {
                    note: NoteRef::new(channel, key),
                    velocity: f32::from(velocity) / 127.0,
                };
                let output = render(&mut sampler, 1, 4_800, &[(0, kind)], &[256]).remove(0);
                assert_eq!(
                    heard(&output[480..]),
                    expected(channel, key, velocity),
                    "channel {channel}, key {key}, velocity {velocity}"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 2 * (43 + 10) * 7);
}

#[test]
fn note_offset_shifts_incoming_notes_before_regions_are_chosen() {
    let mut source = source();
    let text = format!("<control> note_offset=2 octave_offset=-1\n{TEXT}");
    let mut sampler = sampler_with(&text, &mut source, 16, 1);
    // Key 70 becomes 60: regions 2 and 3 at velocity 127.
    let output = render_mono(&mut sampler, 4_800, &[(0, on(70, 1.0))]);
    assert_eq!(heard(&output[480..]), [2, 3]);
    // Key 5 becomes -5: nothing.
    sampler.reset();
    let output = render_mono(&mut sampler, 4_800, &[(0, on(5, 1.0))]);
    assert_eq!(heard(&output[480..]), Vec::<usize>::new());
}

#[test]
fn a_note_on_with_velocity_zero_is_a_note_off() {
    let mut source = source();
    let mut sampler = sampler_with("<region> sample=r2.wav ampeg_release=0", &mut source, 16, 1);
    let output = render_mono(&mut sampler, 960, &[(0, on(60, 1.0)), (480, on(60, 0.0))]);
    assert!(output[..480].iter().any(|&s| s.abs() > 0.4));
    assert!(output[481..].iter().all(|&s| s == 0.0));
    assert_eq!(sampler.active_voices(), 0);
}

#[test]
fn a_key_of_minus_one_keeps_a_region_off_the_keyboard() {
    // SFZ 2: `hikey=-1` (or `key=-1`) means no key plays the region;
    // `lokey=-1` alone is below every key, so it changes nothing.
    let mut source = MemorySource::new().with("dc.wav", constant_wav(0.5, 4_800, 48_000));
    for (text, sounds) in [
        ("<region> sample=dc.wav lokey=-1 hikey=-1", false),
        ("<region> sample=dc.wav hikey=-1", false),
        ("<region> sample=dc.wav key=-1", false),
        ("<group> hikey=-1 <region> sample=dc.wav hikey=0", true),
        ("<region> sample=dc.wav lokey=-1 hikey=0", true),
    ] {
        let mut sampler = sampler_with(text, &mut source, 4, 1);
        let output = render_mono(
            &mut sampler,
            480,
            &[(0, on(0, 1.0)), (0, on(1, 1.0)), (0, on(127, 1.0))],
        );
        let heard = output.iter().any(|&s| s != 0.0);
        assert_eq!(heard, sounds, "{text}");
        // Key 0 only, when it sounds.
        if sounds {
            assert_eq!(sampler.active_voices(), 1, "{text}");
        }
    }
    let sfz = manguebyte_sampler::Sfz::parse("<region> sample=a.wav key=-1 lokey=-3").unwrap();
    let region = &sfz.regions[0];
    assert_eq!((region.lokey, region.hikey), (-1, -1));
    // `key=-1` leaves the pitch centre alone, and -3 is held to -1.
    assert_eq!(
        region.pitch_keycenter,
        manguebyte_sampler::KeyCenter::Key(60)
    );
    assert_eq!(sfz.warnings.len(), 1);
}
