//! Events land on their frame: exactly when they are on time, at the start
//! of the next quantum when they are late, and later when they are ahead.

mod common;

use common::*;
use manguebyte_web::{
    Chain, DAMP_SECONDS, Drum, Engine, EngineConfig, EventRecord, HAT_CHOKE_SECONDS,
    InstrumentKind, Pushed, event_ring, ids,
};

const KICK: u8 = 36;
const CLOSED_HAT: u8 = 42;
const OPEN_HAT: u8 = 46;

/// `output` is silent before `at` and from there on is `reference`,
/// sample for sample.
fn assert_shifted(output: &[f32], reference: &[f32], at: usize) {
    assert!(
        output[..at].iter().all(|&sample| sample == 0.0),
        "sound before frame {at}: first at {:?}",
        onset(output)
    );
    let length = output.len() - at;
    assert_eq!(bits(&output[at..]), bits(&reference[..length]), "from frame {at}");
}

/// What `kind` plays for `records` over `frames`, in quanta of `quantum`.
fn play(
    kind: InstrumentKind,
    chain: Chain,
    records: &[EventRecord],
    frames: usize,
    quantum: usize,
) -> (Vec<f32>, Vec<f32>) {
    let mut engine = engine_with(kind, chain);
    push_all(&mut engine, records);
    render(&mut engine, frames, quantum)
}

#[test]
fn a_note_lands_on_its_exact_frame_wherever_it_falls_in_a_quantum() {
    const FRAMES: usize = 4_096;
    for kind in [InstrumentKind::Drums, InstrumentKind::Synth] {
        let note = if kind == InstrumentKind::Drums { KICK } else { 45 };
        for chain in [Chain::NONE, Chain::FULL] {
            let (reference, reference_right) =
                play(kind, chain, &[EventRecord::note_on(0, 0, note, 0.9)], FRAMES, 128);
            assert_eq!(onset(&reference).map(|frame| frame <= 1), Some(true));
            for at in [1usize, 2, 127, 128, 129, 255, 300, 1_000] {
                for quantum in [128, 7] {
                    let records = [EventRecord::note_on(at as u64, 0, note, 0.9)];
                    let (left, right) = play(kind, chain, &records, FRAMES, quantum);
                    assert_shifted(&left, &reference, at);
                    assert_shifted(&right, &reference_right, at);
                }
            }
        }
    }
    // The drum's first sample is not zero, so its onset is the frame itself.
    let (left, _) = play(
        InstrumentKind::Drums,
        Chain::NONE,
        &[EventRecord::note_on(300, 0, KICK, 1.0)],
        512,
        128,
    );
    assert_eq!(onset(&left), Some(300));
}

#[test]
fn a_late_event_lands_on_the_first_frame_of_the_next_quantum() {
    const FRAMES: usize = 2_048;
    let reference = play(
        InstrumentKind::Drums,
        Chain::FULL,
        &[EventRecord::note_on(0, 0, KICK, 1.0)],
        FRAMES,
        128,
    )
    .0;
    for (quantum, before) in [(128usize, 256usize), (7, 21), (1, 5), (512, 512)] {
        // Frame 3 has passed, and so has frame `before - 1`.
        for late in [3, before as u64 - 1] {
            let mut engine = engine_with(InstrumentKind::Drums, Chain::FULL);
            let (silent, _) = render(&mut engine, before, quantum);
            assert_eq!(push_one(&mut engine, EventRecord::note_on(late, 0, KICK, 1.0)), Pushed::Queued);
            let (rest, _) = render(&mut engine, FRAMES - before, quantum);
            let mut left = silent;
            left.extend(rest);
            assert_shifted(&left, &reference, before);
            assert_eq!(engine.stats().late, 1);
        }
    }
}

fn push_one(engine: &mut Engine, record: EventRecord) -> Pushed {
    engine.push(record)
}

#[test]
fn an_event_far_ahead_waits_for_its_frame() {
    let at = 50_000usize;
    let reference = play(
        InstrumentKind::Drums,
        Chain::FULL,
        &[EventRecord::note_on(0, 0, KICK, 1.0)],
        10_000,
        128,
    )
    .0;
    let mut engine = engine_with(InstrumentKind::Drums, Chain::FULL);
    push_all(&mut engine, &[EventRecord::note_on(at as u64, 0, KICK, 1.0)]);
    let (before, _) = render(&mut engine, 49_920, 128);
    assert!(before.iter().all(|&sample| sample == 0.0));
    assert_eq!(engine.pending_len(), 1);
    let (after, _) = render(&mut engine, 10_080, 128);
    assert_eq!(engine.pending_len(), 0);
    let mut left = before;
    left.extend(after);
    assert_shifted(&left, &reference, at);
    assert_eq!(engine.stats().late, 0);
}

#[test]
fn events_on_one_frame_apply_in_the_order_they_were_pushed() {
    let on = EventRecord::note_on(100, 0, KICK, 1.0);
    let choke = EventRecord::choke(100, 0);
    // Pushed out of frame order, too: a later frame first.
    let later = EventRecord::note_on(400, 0, KICK, 1.0);
    let (silenced, _) = play(InstrumentKind::Drums, Chain::NONE, &[later, on, choke], 512, 64);
    assert_eq!(onset(&silenced), Some(400));
    let (sounding, _) = play(InstrumentKind::Drums, Chain::NONE, &[later, choke, on], 512, 64);
    assert_eq!(onset(&sounding), Some(100));
}

#[test]
fn a_parameter_changes_on_its_frame() {
    const FRAMES: usize = 6_000;
    const AT: usize = 1_000;
    let note = [EventRecord::note_on(0, 0, 45, 1.0)];
    let (reference, _) = play(InstrumentKind::Synth, Chain::NONE, &note, FRAMES, 128);
    for quantum in [128, 7, 1] {
        let records = [
            note[0],
            EventRecord::param(AT as u64, 0, id(ids::GAIN), -60.0),
        ];
        let (left, _) = play(InstrumentKind::Synth, Chain::NONE, &records, FRAMES, quantum);
        assert_eq!(bits(&left[..AT]), bits(&reference[..AT]));
        assert_ne!(left[AT].to_bits(), reference[AT].to_bits());
        // A 20 ms straight fade: 960 frames, the last of them at gain 0.
        let ramp = (0.020 * RATE) as usize;
        assert!(left[AT + ramp - 2] != 0.0);
        assert!(left[AT + ramp - 1..].iter().all(|&sample| sample == 0.0));
    }

    // The stored value is clamped to the range, as the description says.
    let mut engine = engine_with(InstrumentKind::Synth, Chain::FULL);
    push_all(
        &mut engine,
        &[
            EventRecord::param(0, 0, id(ids::GAIN), 1e9),
            EventRecord::param(0, 0, id(ids::LOWPASS_CUTOFF), 440.0),
            EventRecord::param(0, 0, id(ids::SYNTH_WAVE), 2.4),
        ],
    );
    render(&mut engine, 1, 1);
    assert_eq!(engine.param_value(0, ids::GAIN), Some(12.0));
    assert_eq!(engine.param_value(0, ids::LOWPASS_CUTOFF), Some(440.0));
    assert_eq!(engine.param_value(0, ids::SYNTH_WAVE), Some(2.0));
    assert_eq!(engine.stats().unrouted, 0);
}

#[test]
fn a_chain_parameter_changes_the_sound_from_its_frame() {
    const AT: usize = 2_000;
    let note = [EventRecord::note_on(0, 0, 45, 1.0)];
    let (reference, _) = play(InstrumentKind::Synth, Chain::FULL, &note, 4_000, 128);
    for name in [ids::LOWPASS_CUTOFF, ids::LOWPASS_RESONANCE, ids::REVERB_MIX, ids::PAN, ids::SYNTH_CUTOFF] {
        let value = if name.ends_with("cutoff") { 200.0 } else { 0.9 };
        let records = [note[0], EventRecord::param(AT as u64, 0, id(name), value)];
        let (left, _) = play(InstrumentKind::Synth, Chain::FULL, &records, 4_000, 7);
        assert_eq!(bits(&left[..AT]), bits(&reference[..AT]), "{name}");
        let changed = (AT..4_000).find(|&frame| left[frame].to_bits() != reference[frame].to_bits());
        assert_eq!(changed, Some(AT), "{name}");
    }
}

#[test]
fn choke_stops_every_note_at_once() {
    const AT: usize = 3_000;
    for (kind, notes) in [
        (InstrumentKind::Synth, vec![45u8, 52, 57, 60]),
        (InstrumentKind::Drums, vec![KICK, OPEN_HAT, 49, 51]),
    ] {
        let mut records: Vec<EventRecord> =
            notes.iter().map(|&note| EventRecord::note_on(0, 0, note, 1.0)).collect();
        let (reference, _) = play(kind, Chain::NONE, &records, 8_000, 128);
        records.push(EventRecord::choke(AT as u64, 0));
        let mut engine = engine_with(kind, Chain::NONE);
        push_all(&mut engine, &records);
        let (left, right) = render(&mut engine, 8_000, 100);
        assert_eq!(bits(&left[..AT]), bits(&reference[..AT]));
        assert!(reference[AT..].iter().any(|&sample| sample != 0.0));
        assert!(left[AT..].iter().chain(&right[AT..]).all(|&sample| sample == 0.0));
        assert_eq!(engine.active_voices(0), Some(0));
    }
}

#[test]
fn all_notes_off_releases_every_note_of_the_synth() {
    const AT: u64 = 3_000;
    let notes = [45u8, 52, 57];
    let on: Vec<EventRecord> = notes.iter().map(|&note| EventRecord::note_on(0, 0, note, 0.8)).collect();
    let mut released = on.clone();
    released.extend(notes.iter().map(|&note| EventRecord::note_off(AT, 0, note)));
    let mut all_off = on.clone();
    all_off.push(EventRecord::all_notes_off(AT, 0));
    let (by_hand, _) = play(InstrumentKind::Synth, Chain::FULL, &released, 30_000, 128);
    let mut engine = engine_with(InstrumentKind::Synth, Chain::FULL);
    push_all(&mut engine, &all_off);
    let (left, _) = render(&mut engine, 30_000, 128);
    assert_eq!(bits(&left), bits(&by_hand));
    // A release tail, then silence: the release is 0.25 s to -60 dB.
    assert!(left[AT as usize + 2_000] != 0.0);
    assert_eq!(engine.active_voices(0), Some(0));
    // Other parts are not touched.
    let mut engine = common::engine();
    engine.create_part(0, InstrumentKind::Synth).unwrap();
    engine.create_part(1, InstrumentKind::Synth).unwrap();
    push_all(
        &mut engine,
        &[
            EventRecord::note_on(0, 0, 45, 1.0),
            EventRecord::note_on(0, 1, 45, 1.0),
            EventRecord::all_notes_off(10, 1),
            EventRecord::choke(10, 1),
        ],
    );
    render(&mut engine, 30_000, 128);
    assert_eq!(engine.active_voices(0), Some(1));
    assert_eq!(engine.active_voices(1), Some(0));
}

#[test]
fn all_notes_off_damps_the_drums() {
    const AT: usize = 2_000;
    let damp = (DAMP_SECONDS * RATE).round() as usize;
    let ring = [EventRecord::note_on(0, 0, OPEN_HAT, 1.0), EventRecord::note_on(0, 0, 49, 1.0)];
    let (reference, _) = play(InstrumentKind::Drums, Chain::NONE, &ring, 12_000, 128);
    let mut records = ring.to_vec();
    records.push(EventRecord::all_notes_off(AT as u64, 0));
    // A note off does nothing to a drum.
    records.push(EventRecord::note_off(AT as u64 - 10, 0, OPEN_HAT));
    let (left, _) = play(InstrumentKind::Drums, Chain::NONE, &records, 12_000, 64);
    assert_eq!(bits(&left[..AT]), bits(&reference[..AT]));
    assert!(left[AT + damp - 1] != 0.0);
    assert!(left[AT + damp..].iter().all(|&sample| sample == 0.0));
    assert!(reference[AT + damp..].iter().any(|&sample| sample != 0.0));
}

#[test]
fn the_closed_hat_chokes_the_open_hat() {
    const AT: usize = 5_000;
    let choke = (HAT_CHOKE_SECONDS * RATE).round() as usize;
    assert_eq!(choke, 96);
    let open = EventRecord::note_on(0, 0, OPEN_HAT, 1.0);
    let closed = EventRecord::note_on(AT as u64, 0, CLOSED_HAT, 0.7);
    let (both, _) = play(InstrumentKind::Drums, Chain::NONE, &[open, closed], 20_000, 128);
    let (closed_alone, _) = play(InstrumentKind::Drums, Chain::NONE, &[closed], 20_000, 128);
    let (open_alone, _) = play(InstrumentKind::Drums, Chain::NONE, &[open], 20_000, 128);
    assert_eq!(bits(&both[..AT]), bits(&open_alone[..AT]));
    // During the fade both hats are heard, after it only the closed one.
    assert!((AT..AT + choke).all(|frame| both[frame] != closed_alone[frame]));
    assert_eq!(bits(&both[AT + choke..]), bits(&closed_alone[AT + choke..]));
    // Without the closed hat, the open one rings on.
    assert!(open_alone[AT + choke..AT + 10_000].iter().any(|&sample| sample.abs() > 1e-3));
    // Other drums are not choked.
    let ride = EventRecord::note_on(0, 0, Drum::Ride.note(), 1.0);
    let (ride_and_hat, _) = play(InstrumentKind::Drums, Chain::NONE, &[ride, closed], 20_000, 128);
    let (ride_alone, _) = play(InstrumentKind::Drums, Chain::NONE, &[ride], 20_000, 128);
    assert!((AT + choke..20_000).any(|frame| ride_and_hat[frame] != closed_alone[frame]));
    assert_ne!(bits(&ride_and_hat[AT..]), bits(&ride_alone[AT..]));
}

#[test]
fn records_out_of_range_are_ignored_and_counted() {
    let mut engine = engine_with(InstrumentKind::Synth, Chain::FULL);
    let good = EventRecord::note_on(0, 0, 60, 0.5);
    let hostile = [
        EventRecord { kind: 0, ..good },
        EventRecord { kind: 6, ..good },
        EventRecord { kind: 255, ..good },
        EventRecord { note: 128, ..good },
        EventRecord { note: 255, ..good },
        EventRecord { channel: 16, ..good },
        EventRecord { velocity: -0.1, ..good },
        EventRecord { velocity: 1.5, ..good },
        EventRecord { velocity: f32::NAN, ..good },
        EventRecord { velocity: f32::INFINITY, ..good },
        EventRecord::note_off(0, 0, 200),
        EventRecord::param(0, 0, id(ids::GAIN), f64::NAN),
        EventRecord::param(0, 0, id(ids::GAIN), f64::INFINITY),
        EventRecord::param(0, 0, id(ids::GAIN), f64::NEG_INFINITY),
    ];
    for record in hostile {
        assert_eq!(engine.push(record), Pushed::Ignored, "{record:?}");
    }
    assert_eq!(engine.stats().ignored, hostile.len() as u64);
    assert_eq!(engine.pending_len(), 0);
    // Fields a kind does not use may hold anything.
    let odd = [
        EventRecord { note: 250, channel: 99, velocity: f32::NAN, ..EventRecord::choke(0, 0) },
        EventRecord { value: f64::NAN, ..EventRecord::all_notes_off(0, 0) },
        EventRecord { velocity: f32::NAN, ..EventRecord::note_off(0, 0, 60) },
        EventRecord { note: 255, velocity: f32::NAN, ..EventRecord::param(0, 0, id(ids::PAN), 0.5) },
    ];
    for record in odd {
        assert_eq!(engine.push(record), Pushed::Queued, "{record:?}");
    }
    // Missing parts and parameters are counted when the event is applied.
    push_all(
        &mut engine,
        &[
            EventRecord::note_on(0, 7, 60, 0.5),
            EventRecord::param(0, 0, id("no.such.param"), 1.0),
            EventRecord::param(0, 0, id(ids::KICK_LEVEL), 1.0),
        ],
    );
    render(&mut engine, 4, 4);
    let stats = engine.stats();
    assert_eq!((stats.applied, stats.unrouted), (7, 3));
    assert_eq!(engine.param_value(0, ids::PAN), Some(0.5));
}

#[test]
fn a_velocity_of_zero_is_a_note_off() {
    let records = [EventRecord::note_on(0, 0, 45, 1.0), EventRecord::note_on(1_000, 0, 45, 0.0)];
    let by_note_off = [records[0], EventRecord::note_off(1_000, 0, 45)];
    let (a, _) = play(InstrumentKind::Synth, Chain::FULL, &records, 30_000, 128);
    let (b, _) = play(InstrumentKind::Synth, Chain::FULL, &by_note_off, 30_000, 128);
    assert_eq!(bits(&a), bits(&b));
}

#[test]
fn a_full_queue_drops_what_does_not_fit_and_a_ring_keeps_it() {
    let config = EngineConfig {
        pending_capacity: 4,
        ..EngineConfig::new(RATE)
    };
    let mut engine = Engine::new(config).unwrap();
    engine.create_part(0, InstrumentKind::Drums).unwrap();
    for frame in 0..4 {
        assert_eq!(engine.push(EventRecord::note_on(frame, 0, KICK, 1.0)), Pushed::Queued);
    }
    assert_eq!(engine.push(EventRecord::note_on(9, 0, KICK, 1.0)), Pushed::Full);
    assert_eq!(engine.stats().dropped, 1);
    assert_eq!(engine.pending_room(), 0);
    engine.clear_pending();
    assert_eq!(engine.pending_room(), 4);

    // Drained from a ring, what does not fit stays in the ring.
    let (mut producer, mut consumer) = event_ring(16);
    for frame in 0..10 {
        producer.push(EventRecord::note_on(frame * 10, 0, KICK, 1.0)).unwrap();
    }
    assert_eq!(engine.drain(&mut consumer), 4);
    assert_eq!(consumer.len(), 6);
    assert_eq!(engine.drain(&mut consumer), 0);
    render(&mut engine, 35, 35); // frames 0, 10, 20 and 30 are applied
    assert_eq!(engine.drain(&mut consumer), 4);
    assert_eq!(engine.drain(&mut consumer), 0);
    render(&mut engine, 40, 40); // frames 35 to 74: 40, 50, 60 and 70
    assert_eq!(engine.drain(&mut consumer), 2);
    assert!(consumer.is_empty());
    assert_eq!(engine.stats().dropped, 1);
    assert_eq!(engine.stats().late, 0);
}

#[test]
fn records_copied_in_a_message_are_pushed_whole() {
    let mut engine = engine_with(InstrumentKind::Drums, Chain::NONE);
    let mut bytes = Vec::new();
    for record in [
        EventRecord::note_on(10, 0, KICK, 1.0),
        EventRecord { kind: 77, ..EventRecord::default() },
        EventRecord::note_on(20, 0, CLOSED_HAT, 1.0),
    ] {
        bytes.extend_from_slice(&record.to_bytes());
    }
    bytes.extend_from_slice(&[1, 2, 3]); // a torn record at the end
    assert_eq!(engine.push_bytes(&bytes), 2);
    assert_eq!(engine.stats().ignored, 1);
    let (left, _) = render(&mut engine, 64, 64);
    assert_eq!(onset(&left), Some(10));
}

#[test]
fn parts_are_made_replaced_and_removed_by_control_calls() {
    let config = EngineConfig {
        max_parts: 2,
        ..EngineConfig::new(RATE)
    };
    let mut engine = Engine::new(config).unwrap();
    engine.create_part(9, InstrumentKind::Synth).unwrap();
    engine.create_part(3, InstrumentKind::Drums).unwrap();
    assert_eq!(engine.part_ids().collect::<Vec<_>>(), [3, 9]);
    assert_eq!(
        engine.create_part(4, InstrumentKind::Drums),
        Err(manguebyte_web::EngineError::TooManyParts { max: 2 })
    );
    // Replacing a part is allowed when full, and starts it afresh.
    push_all(&mut engine, &[EventRecord::param(0, 9, id(ids::GAIN), -6.0)]);
    render(&mut engine, 1, 1);
    assert_eq!(engine.param_value(9, ids::GAIN), Some(-6.0));
    engine.create_part(9, InstrumentKind::Drums).unwrap();
    assert_eq!(engine.part_kind(9), Some(InstrumentKind::Drums));
    assert_eq!(engine.param_value(9, ids::GAIN), Some(0.0));
    engine.remove_part(3).unwrap();
    assert_eq!(
        engine.remove_part(3),
        Err(manguebyte_web::EngineError::NoSuchPart(3))
    );
    assert_eq!(engine.set_chain(3, Chain::NONE), Err(manguebyte_web::EngineError::NoSuchPart(3)));
    assert_eq!(engine.part_ids().collect::<Vec<_>>(), [9]);

    // A stage taken out of the chain loses its parameters from the
    // description and its events go nowhere.
    engine.set_chain(9, Chain { lowpass: false, reverb: true }).unwrap();
    assert_eq!(engine.part_chain(9), Some(Chain { lowpass: false, reverb: true }));
    let json = engine.params_json(9).unwrap();
    assert!(!json.contains(ids::LOWPASS_CUTOFF));
    assert!(json.contains(ids::REVERB_MIX));
    push_all(&mut engine, &[EventRecord::param(0, 9, id(ids::LOWPASS_CUTOFF), 100.0)]);
    let before = engine.stats().unrouted;
    render(&mut engine, 1, 1);
    assert_eq!(engine.stats().unrouted, before + 1);
    engine.set_chain(9, Chain::FULL).unwrap();
    assert_eq!(engine.param_value(9, ids::LOWPASS_CUTOFF), Some(20_000.0));
}
