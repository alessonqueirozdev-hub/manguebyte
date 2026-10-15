//! Helpers shared by the integration tests.

#![allow(dead_code)]

use manguebyte_web::{Chain, Engine, EngineConfig, EventRecord, InstrumentKind, ParamId, Pushed};

pub const RATE: f64 = 48_000.0;

/// An engine at 48 kHz with the default limits.
pub fn engine() -> Engine {
    Engine::new(EngineConfig::new(RATE)).unwrap()
}

/// An engine with one part, `0`, of `kind` through `chain`.
pub fn engine_with(kind: InstrumentKind, chain: Chain) -> Engine {
    let mut engine = engine();
    engine.create_part_with(0, kind, chain).unwrap();
    engine
}

/// Renders `frames` frames in quanta of `quantum` and returns both channels.
pub fn render(engine: &mut Engine, frames: usize, quantum: usize) -> (Vec<f32>, Vec<f32>) {
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    while left.len() < frames {
        let count = engine.render(quantum.min(frames - left.len()));
        let (l, r) = engine.output();
        left.extend_from_slice(l);
        right.extend_from_slice(r);
        assert_eq!(count, l.len());
    }
    (left, right)
}

/// Pushes every record, insisting each one is queued.
pub fn push_all(engine: &mut Engine, records: &[EventRecord]) {
    for &record in records {
        assert_eq!(engine.push(record), Pushed::Queued, "{record:?}");
    }
}

pub fn id(name: &str) -> ParamId {
    ParamId::from_name(name)
}

pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()))
}

pub fn bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|sample| sample.to_bits()).collect()
}

/// The first frame that is not exactly zero.
pub fn onset(samples: &[f32]) -> Option<usize> {
    samples.iter().position(|&sample| sample != 0.0)
}
