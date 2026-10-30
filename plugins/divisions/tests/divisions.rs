//! Each division of Mangue Divisions sounds on its own bus and nowhere
//! else, the main output carries them all, and a bus switched off changes
//! nothing but itself.

use mangue_divisions::{DIVISIONS, Divisions};
use manguebyte::dsp::event::{EventKind, NoteRef};
use manguebyte::prelude::*;

const RATE: f64 = 48_000.0;
const FRAMES: usize = 4_800;

/// A note on `channel` from frame 100, held for most of the render.
fn play(harness: &mut Harness<Divisions>, channel: u8, key: u8) {
    let note = NoteRef::new(channel, key);
    harness.schedule(
        100,
        EventKind::NoteOn {
            note,
            velocity: 0.8,
        },
    );
    harness.schedule(
        4_000,
        EventKind::NoteOff {
            note,
            velocity: 0.0,
        },
    );
}

fn peak(buffer: &AudioBuffer<f32>) -> f32 {
    buffer.peak()
}

#[test]
fn each_division_sounds_on_its_own_bus_alone() {
    for division in 0..DIVISIONS.len() {
        let mut harness = Harness::<Divisions>::new(RATE, 256).with_block_pattern(&[256, 61]);
        play(&mut harness, division as u8, 60);
        let main = harness.render(FRAMES);
        assert!(peak(&main) > 0.01, "division {division} is heard");
        for bus in 0..DIVISIONS.len() {
            let written = harness.aux_output(bus).expect("every bus is on");
            assert_eq!(written.num_channels(), 2);
            if bus == division {
                assert!(peak(written) > 0.01, "division {division} on its bus");
            } else {
                assert_eq!(peak(written), 0.0, "division {division} not on bus {bus}");
            }
        }
    }
}

#[test]
fn the_main_output_is_every_division_together() {
    let mut harness = Harness::<Divisions>::new(RATE, 256);
    for (channel, key) in [(0, 60), (1, 64), (2, 67), (3, 36), (9, 72)] {
        play(&mut harness, channel, key);
    }
    let main = harness.render(FRAMES);
    for channel in 0..2 {
        for frame in 0..FRAMES {
            let sum: f32 = (0..DIVISIONS.len())
                .map(|bus| harness.aux_output(bus).unwrap().channel(channel)[frame])
                .sum();
            let heard = main.channel(channel)[frame];
            assert!(
                (heard - sum).abs() <= 1e-6,
                "frame {frame}: {heard} against {sum}"
            );
        }
    }
    // Channel 10 plays the Great, with channel 1.
    let great = harness.aux_output(0).unwrap();
    assert!(peak(great) > peak(harness.aux_output(2).unwrap()));
}

#[test]
fn a_bus_switched_off_leaves_the_main_output_as_it_was() {
    let render = |off: bool| {
        let mut harness = Harness::<Divisions>::new(RATE, 128);
        if off {
            harness.set_aux_active(1, false);
        }
        play(&mut harness, 1, 62);
        let main = harness.render(FRAMES);
        (main, harness.aux_output(1).is_some())
    };
    let (on, heard_on) = render(false);
    let (off, heard_off) = render(true);
    assert!(heard_on && !heard_off);
    assert_eq!(on, off, "the swell is in the main output either way");
}
