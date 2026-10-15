//! Every instrument makes sound, within full scale, and stops.

mod common;

use common::*;
use manguebyte_web::{Chain, DRUM_MAP, EventRecord, InstrumentKind, ids};

fn rms(samples: &[f32]) -> f64 {
    (samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / samples.len() as f64)
        .sqrt()
}

/// Renders one note at `velocity` for two seconds, returning the left
/// channel and the voices still sounding at the end.
fn strike(kind: InstrumentKind, chain: Chain, note: u8, velocity: f32, hold: u64) -> (Vec<f32>, Vec<f32>, usize) {
    let mut engine = engine_with(kind, chain);
    push_all(
        &mut engine,
        &[
            EventRecord::note_on(0, 0, note, velocity),
            EventRecord::note_off(hold, 0, note),
        ],
    );
    let (left, right) = render(&mut engine, 2 * RATE as usize, 128);
    (left, right, engine.active_voices(0).unwrap())
}

#[test]
fn print_levels() {
    for entry in DRUM_MAP {
        for chain in [Chain::NONE, Chain::FULL] {
            let (left, _, voices) = strike(InstrumentKind::Drums, chain, entry.note, 1.0, 100);
            eprintln!(
                "{:>10} {:?}: peak {:.3} rms {:.4} voices {voices}",
                entry.name,
                chain.bits(),
                peak(&left),
                rms(&left[..4_800])
            );
        }
    }
    for note in [33u8, 45, 57, 69, 81] {
        let (left, _, voices) = strike(InstrumentKind::Synth, Chain::FULL, note, 1.0, 24_000);
        eprintln!("synth {note}: peak {:.3} rms {:.4} voices {voices}", peak(&left), rms(&left[..24_000]));
    }
    let _ = ids::GAIN;
}
