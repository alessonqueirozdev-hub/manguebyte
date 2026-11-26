//! A `Synth` spending a processor budget (ADR 0018): what it demotes, cuts,
//! suspends and steals, and that without a budget, with room enough in it
//! or offline, it renders what it would have, bit for bit.
//!
//! Governors here run on a clock the voices move as they render, so every
//! figure is the same on every machine.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use manguebyte_core::AudioBuffer;
use manguebyte_core::governor::{Allowance, Clock, Governor, GovernorConfig};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::instruments::{OrganParams, OrganVoice, PluckedString, StringParams};
use manguebyte_dsp::{
    BUDGET_FADE_SECONDS, ProcessContext, ProcessSpec, Processor, Synth, Transport, Tuning, Voice,
};
use manguebyte_midi::MidiMessage;

const RATE: f64 = 48_000.0;
const BLOCK: usize = 128;
/// The fade of a cut voice, in frames at the rate the tests run at.
const FADE: usize = 288;

fn spec(max_block_size: usize) -> ProcessSpec {
    ProcessSpec {
        sample_rate: RATE,
        max_block_size,
        num_channels: 1,
    }
}

fn on(key: u8) -> EventKind {
    EventKind::NoteOn {
        note: NoteRef::new(0, key),
        velocity: 1.0,
    }
}

fn off(key: u8) -> EventKind {
    EventKind::NoteOff {
        note: NoteRef::new(0, key),
        velocity: 0.0,
    }
}

/// How a block is rendered.
#[derive(Clone, Copy)]
enum How {
    /// `process_with`, as ever.
    Free,
    /// Offline, whatever the allowance (here none at all).
    Offline,
    /// Within this many units.
    Within(f64),
}

/// Renders `frames` frames in blocks of `block`, with the events of
/// `script` on their frames, `how(frame)` saying how each block is.
fn play<V: Voice>(
    synth: &mut Synth<V>,
    script: &[(usize, EventKind)],
    frames: usize,
    block: usize,
    how: impl Fn(usize) -> How,
) -> Vec<f32> {
    let mut output = Vec::with_capacity(frames);
    let mut events = EventBuffer::with_capacity(64);
    let mut buffer = AudioBuffer::<f32>::new(1, block);
    let mut at = 0;
    while at < frames {
        let length = block.min(frames - at);
        events.clear();
        for &(frame, kind) in script {
            if (at..at + length).contains(&frame) {
                let offset = (frame - at) as u32;
                events.push(Event { offset, kind }).unwrap();
            }
        }
        buffer.set_num_frames(length);
        let context = ProcessContext::new(&events, &Transport::STOPPED);
        match how(at) {
            How::Free => synth.process_with(&mut buffer, &context),
            How::Offline => synth.process_budgeted(
                &mut buffer,
                &context.with_offline(true),
                &mut Allowance::new(0.0),
            ),
            How::Within(units) => {
                synth.process_budgeted(&mut buffer, &context, &mut Allowance::new(units));
            }
        }
        output.extend_from_slice(buffer.channel(0));
        at += length;
    }
    output
}

fn organ(voices: usize) -> Synth<OrganVoice> {
    let mut synth = Synth::new(OrganVoice::voices(voices, OrganParams::full()));
    synth.prepare(&spec(1_000));
    synth
}

/// Notes struck, released, struck again, bent and held by the pedal.
fn phrase() -> Vec<(usize, EventKind)> {
    let controller = |controller, value| {
        EventKind::Midi(MidiMessage::ControlChange {
            channel: 0,
            controller,
            value,
        })
    };
    let bend = EventKind::Midi(MidiMessage::PitchBend {
        channel: 0,
        value: 3_000,
    });
    vec![
        (0, on(48)),
        (1, on(55)),
        (700, on(64)),
        (901, off(48)),
        (950, bend),
        (1_200, on(48)),
        (2_000, controller(64, 127)),
        (2_100, off(55)),
        (2_600, on(67)),
        (2_700, on(72)),
        (3_000, controller(64, 0)),
        (3_100, off(64)),
        (
            4_000,
            EventKind::NoteChoke {
                note: NoteRef::new(0, 67),
            },
        ),
        (4_500, off(48)),
    ]
}

#[test]
fn a_budget_that_covers_every_voice_changes_nothing() {
    fn check<V: Voice>(what: &str, make: impl Fn() -> Synth<V>) {
        for block in [1, 7, 64, 256, 1_000] {
            let run = |how: How| play(&mut make(), &phrase(), 8_000, block, move |_| how);
            let free = run(How::Free);
            assert!(free.iter().any(|&s| s != 0.0));
            // Voice count and note stealing included: four voices for a
            // phrase that wants six.
            for how in [How::Within(f64::INFINITY), How::Within(1e6), How::Offline] {
                assert!(run(how) == free, "{what}, blocks of {block}");
            }
        }
    }
    check("organ", || organ(4));
    check("strings", || {
        let mut synth = Synth::with_voices(4, || PluckedString::new(StringParams::nylon_guitar()));
        synth.prepare(&spec(1_000));
        synth
    });
}

/// A clock the voices move.
#[derive(Clone, Default)]
struct FakeClock(Rc<Time>);

#[derive(Default)]
struct Time {
    /// Nanoseconds gone by.
    now: Cell<f64>,
    reads: Cell<u64>,
    /// The nanoseconds a unit takes for each frame.
    per_unit_frame: Cell<f64>,
}

impl Clock for FakeClock {
    fn now(&mut self) -> Duration {
        self.0.reads.set(self.0.reads.get() + 1);
        Duration::from_nanos(self.0.now.get() as u64)
    }
}

impl FakeClock {
    /// Sets the load one unit puts on a block, as a fraction of its
    /// deadline.
    fn set_unit_load(&self, load: f64) {
        self.0.per_unit_frame.set(load * 1e9 / RATE);
    }
}

/// A framework voice whose rendering takes, on the fake clock, what its
/// tier costs.
struct Timed<V> {
    voice: V,
    tier: usize,
    clock: FakeClock,
}

impl<V: Voice> Voice for Timed<V> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.voice.prepare(spec);
    }
    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        self.voice.note_on(note, velocity);
    }
    fn note_off(&mut self, velocity: f32) {
        self.voice.note_off(velocity);
    }
    fn choke(&mut self) {
        self.tier = 0;
        self.voice.choke();
    }
    fn is_active(&self) -> bool {
        self.voice.is_active()
    }
    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        let time = &self.clock.0;
        let units = self.voice.tier_cost(self.tier);
        time.now
            .set(time.now.get() + units * buffer.num_frames() as f64 * time.per_unit_frame.get());
        self.voice.render(buffer);
    }
    fn expression(&mut self, expression: Expression, value: f64) {
        self.voice.expression(expression, value);
    }
    fn set_tuning(&mut self, tuning: &Tuning) {
        self.voice.set_tuning(tuning);
    }
    fn tier_count(&self) -> usize {
        self.voice.tier_count()
    }
    fn tier_cost(&self, tier: usize) -> f64 {
        self.voice.tier_cost(tier)
    }
    fn set_tier(&mut self, tier: usize, fade_frames: usize) {
        self.tier = tier;
        self.voice.set_tier(tier, fade_frames);
    }
    fn audibility(&self) -> f32 {
        self.voice.audibility()
    }
    fn can_suspend(&self) -> bool {
        self.voice.can_suspend()
    }
    fn skip_suspended(&mut self, frames: usize) {
        self.voice.skip_suspended(frames);
    }
}

fn timed_organ(clock: &FakeClock, voices: usize) -> Synth<Timed<OrganVoice>> {
    let mut synth = Synth::new(
        OrganVoice::voices(voices, OrganParams::full())
            .into_iter()
            .map(|voice| Timed {
                voice,
                tier: 0,
                clock: clock.clone(),
            }),
    );
    synth.prepare(&spec(BLOCK));
    synth
}

#[test]
fn voices_degrade_under_a_small_budget_and_recover() {
    const NOTES: usize = 16;
    let demand = NOTES as f64 * OrganVoice::FULL_COST;
    let clock = FakeClock::default();
    // A machine on which the whole chord takes two deadlines.
    clock.set_unit_load(2.0 / demand);
    let mut synth = timed_organ(&clock, NOTES);
    let mut governor = Governor::with_clock(GovernorConfig::default(), clock.clone());
    for key in 0..NOTES as u8 {
        synth.note_on(NoteRef::new(0, 36 + 2 * key), 1.0);
    }
    let mut block = AudioBuffer::<f32>::new(1, BLOCK);
    let mut run = |synth: &mut Synth<Timed<OrganVoice>>,
                   governor: &mut Governor<FakeClock>,
                   blocks: usize| {
        for _ in 0..blocks {
            synth.process_governed(&mut block, &ProcessContext::empty(), governor);
            assert!(block.peak() > 0.0 && block.peak().is_finite());
        }
    };

    // The first block learns the cost, rendering everything.
    run(&mut synth, &mut governor, 1);
    assert!((governor.last_load() - 2.0).abs() < 1e-6);
    // From then on it keeps to the target, once the cut voices have faded.
    run(&mut synth, &mut governor, 375);
    assert!(governor.last_load() <= 0.7, "{}", governor.last_load());
    assert!(governor.last_load() > 0.5, "{}", governor.last_load());
    let (sounding, suspended) = (synth.active_voices(), synth.suspended_voices());
    assert_eq!(sounding + suspended, NOTES, "no held note is lost");
    assert!(suspended >= NOTES / 2, "{suspended}");
    assert!(sounding >= 4, "{sounding}");

    // A machine four times as fast: the voices come back, slowly.
    clock.set_unit_load(0.5 / demand);
    run(&mut synth, &mut governor, 40);
    assert!(synth.suspended_voices() > 0, "recovery is not at once");
    run(&mut synth, &mut governor, 1_500);
    assert_eq!(
        (
            synth.active_voices(),
            synth.suspended_voices(),
            synth.demoted_voices()
        ),
        (NOTES, 0, 0)
    );
    assert!((governor.last_load() - 0.5).abs() < 1e-3);
    // The first block rendered everything, and the two after it the voices
    // that were fading out.
    assert_eq!(governor.stats().over_deadline, 3);
}

#[test]
fn offline_blocks_render_everything_and_leave_the_governor_alone() {
    let clock = FakeClock::default();
    clock.set_unit_load(1.0);
    let mut governor = Governor::with_clock(GovernorConfig::default(), clock.clone());
    // Even a maximum of next to nothing thins no offline block.
    governor.set_max_budget(1.0);
    let mut governed = timed_organ(&clock, 8);
    let mut free = timed_organ(&FakeClock::default(), 8);
    let mut events = EventBuffer::with_capacity(8);
    for (offset, key) in [(0, 40), (3, 52), (64, 59), (100, 64)] {
        events
            .push(Event {
                offset,
                kind: on(key),
            })
            .unwrap();
    }
    let (mut a, mut b) = (AudioBuffer::new(1, BLOCK), AudioBuffer::new(1, BLOCK));
    for block in 0..20 {
        let context = if block == 0 {
            ProcessContext::new(&events, &Transport::STOPPED)
        } else {
            ProcessContext::empty()
        };
        governed.process_governed(&mut a, &context.with_offline(true), &mut governor);
        free.process_with(&mut b, &context);
        assert_eq!(a.channel(0), b.channel(0), "block {block}");
    }
    assert_eq!(clock.0.reads.get(), 0);
    assert_eq!(governor.stats().blocks, 0);
    // The same governor live holds the synth to its maximum.
    governed.process_governed(&mut a, &ProcessContext::empty(), &mut governor);
    assert_eq!(governed.suspended_voices() + governed.active_voices(), 4);
    assert_eq!(governor.stats().blocks, 1);
}

/// A voice that sounds a constant level, its key's number, with two tiers of
/// 4 units and 1, and notes what it is asked.
#[derive(Default)]
struct Level {
    key: f32,
    audible: f32,
    sounding: bool,
    held: bool,
    waits: bool,
    tier: usize,
    asked: Vec<(usize, usize)>,
    skipped: usize,
}

impl Voice for Level {
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        self.key = f32::from(note.key);
        self.audible = velocity;
        (self.sounding, self.held) = (true, true);
    }
    fn note_off(&mut self, _: f32) {
        self.held = false;
    }
    fn choke(&mut self) {
        (self.sounding, self.held, self.tier) = (false, false, 0);
    }
    fn is_active(&self) -> bool {
        self.sounding
    }
    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        if self.sounding {
            buffer
                .channel_mut(0)
                .iter_mut()
                .for_each(|s| *s += self.key);
        }
    }
    fn tier_count(&self) -> usize {
        2
    }
    fn tier_cost(&self, tier: usize) -> f64 {
        [4.0, 1.0][tier]
    }
    fn set_tier(&mut self, tier: usize, fade_frames: usize) {
        self.tier = tier;
        self.asked.push((tier, fade_frames));
    }
    fn audibility(&self) -> f32 {
        self.audible
    }
    fn can_suspend(&self) -> bool {
        self.waits && self.held
    }
    fn skip_suspended(&mut self, frames: usize) {
        self.skipped += frames;
    }
}

fn levels(voices: usize, waits: bool) -> Synth<Level> {
    let mut synth = Synth::with_voices(voices, || Level {
        waits,
        ..Level::default()
    });
    synth.prepare(&spec(1_000));
    synth
}

/// One block of `frames` within `units`; returns the output and what was
/// spent, and whether it was limited.
fn block(synth: &mut Synth<Level>, frames: usize, units: f64) -> (Vec<f32>, f64, bool) {
    let mut buffer = AudioBuffer::<f32>::new(1, frames);
    let mut allowance = Allowance::new(units);
    synth.process_budgeted(&mut buffer, &ProcessContext::empty(), &mut allowance);
    (
        buffer.channel(0).to_vec(),
        allowance.spent(),
        allowance.limited(),
    )
}

fn tiers(synth: &mut Synth<Level>) -> Vec<usize> {
    synth.voices_mut().map(|voice| voice.tier).collect()
}

#[test]
fn the_budget_goes_to_held_then_louder_then_newer_voices() {
    // Four notes, in the order struck: loud, soft, soft again, and a loud
    // one that is then released.
    let mut synth = levels(4, true);
    for (key, velocity) in [(10, 0.9), (20, 0.2), (30, 0.2), (40, 1.0)] {
        synth.note_on(NoteRef::new(0, key), velocity);
    }
    synth.note_off(NoteRef::new(0, 40), 0.0);
    // Room for one whole and two demoted: the loud held note, then the
    // newer of the two soft ones, then the older; the released one gets
    // nothing and, never heard, is gone.
    let (output, spent, limited) = block(&mut synth, 16, 6.0);
    assert_eq!(tiers(&mut synth)[..3], [0, 1, 1]);
    assert_eq!(output, [60.0; 16]);
    assert_eq!((spent, limited), (6.0, true));
    assert_eq!(
        (synth.active_voices(), synth.demoted_voices()),
        (3, 2),
        "the released note is stolen"
    );
    // New notes are told their tier at once.
    let asked: Vec<_> = synth
        .voices_mut()
        .map(|voice| voice.asked.clone())
        .collect();
    assert_eq!(asked[1], [(1, 0)]);
    assert_eq!(asked[2], [(1, 0)]);

    // Room for one whole and one demoted: the newer of the soft ones.
    let (output, ..) = block(&mut synth, FADE + 16, 5.0);
    assert_eq!(&output[FADE..], [40.0; 16]);
    assert_eq!(tiers(&mut synth)[2], 1);

    // Room for one: the loud one whole, nothing else.
    let (_, spent, _) = block(&mut synth, FADE, 4.0);
    // The other fades out over this block, over budget for as long.
    assert!(spent > 4.0 && spent <= 5.0, "{spent}");
    assert_eq!(synth.suspended_voices(), 2);
    let (output, spent, limited) = block(&mut synth, 16, 4.0);
    assert_eq!((output, spent, limited), (vec![10.0; 16], 4.0, true));

    // Room for everything: they come back to full, fading in.
    let (output, spent, limited) = block(&mut synth, FADE + 16, 100.0);
    assert!(output[0] < 10.0 + 1.0 && output[FADE - 1] == 60.0);
    assert!(output.windows(2).all(|pair| pair[1] >= pair[0]));
    assert_eq!(&output[FADE..], [60.0; 16]);
    assert!(!limited && spent == 12.0, "{spent}");
    assert_eq!((synth.demoted_voices(), synth.suspended_voices()), (0, 0));
}

#[test]
fn a_note_the_pedal_holds_gives_way_to_a_held_key() {
    let pedal = EventKind::Midi(MidiMessage::ControlChange {
        channel: 0,
        controller: 64,
        value: 127,
    });
    let mut synth = levels(2, true);
    synth.handle_event(&Event {
        offset: 0,
        kind: pedal,
    });
    // The pedalled note is the louder and the newer.
    synth.note_on(NoteRef::new(0, 10), 0.1);
    synth.note_on(NoteRef::new(0, 20), 1.0);
    synth.note_off(NoteRef::new(0, 20), 0.0);
    assert_eq!(block(&mut synth, 16, 4.0).0, [10.0; 16]);
}

#[test]
fn a_sounding_voice_crosses_to_another_tier_over_the_fade() {
    assert_eq!(FADE, (BUDGET_FADE_SECONDS * RATE) as usize);
    let mut synth = levels(2, true);
    synth.note_on(NoteRef::new(0, 10), 1.0);
    block(&mut synth, 16, 100.0);
    block(&mut synth, 16, 1.0);
    block(&mut synth, 16, 100.0);
    let voice = synth.voices_mut().next().unwrap();
    assert_eq!(voice.asked, [(1, FADE), (0, FADE)]);
}

#[test]
fn a_voice_that_cannot_wait_is_stolen_and_a_new_note_with_no_room_is_silent() {
    let mut synth = levels(2, false);
    synth.note_on(NoteRef::new(0, 10), 1.0);
    block(&mut synth, 16, 100.0);
    // No room even for the cheapest tier: it fades out, in a straight line,
    // and is gone.
    let (output, spent, limited) = block(&mut synth, FADE + 100, 0.5);
    for (frame, &sample) in output.iter().enumerate() {
        let expected = 10.0 * (1.0 - (frame + 1).min(FADE) as f32 / FADE as f32);
        assert!((sample - expected).abs() < 1e-5, "frame {frame}: {sample}");
    }
    assert!(limited);
    assert!((spent - 4.0 * FADE as f64 / (FADE + 100) as f64).abs() < 1e-9);
    assert_eq!((synth.active_voices(), synth.suspended_voices()), (0, 0));

    // A new note that does not fit makes no sound, and takes no voice.
    synth.note_on(NoteRef::new(0, 20), 1.0);
    let (output, spent, limited) = block(&mut synth, 64, 0.5);
    assert_eq!((output, spent, limited), (vec![0.0; 64], 0.0, true));
    assert_eq!((synth.active_voices(), synth.suspended_voices()), (0, 0));

    // And the voices are free for what comes.
    synth.note_on(NoteRef::new(0, 30), 1.0);
    synth.note_on(NoteRef::new(0, 40), 1.0);
    assert_eq!(block(&mut synth, 4, 100.0).0, [70.0; 4]);
}

#[test]
fn a_suspended_voice_waits_for_the_budget_and_ends_with_its_key() {
    let mut synth = levels(2, true);
    synth.note_on(NoteRef::new(0, 10), 1.0);
    // Never heard, it waits, and is told of the time that passes.
    assert_eq!(block(&mut synth, 100, 0.0).0, [0.0; 100]);
    assert_eq!(block(&mut synth, 50, 0.0).0, [0.0; 50]);
    assert_eq!((synth.active_voices(), synth.suspended_voices()), (0, 1));
    assert_eq!(synth.voices_mut().next().unwrap().skipped, 150);

    // The key struck again while it waits starts it from a clean voice.
    synth.note_on(NoteRef::new(0, 10), 1.0);
    assert_eq!(block(&mut synth, 4, 100.0).0, [10.0; 4]);
    assert_eq!(synth.active_voices(), 1);

    // Released while it waits, it ends.
    block(&mut synth, FADE, 0.0);
    assert_eq!(synth.suspended_voices(), 1);
    synth.note_off(NoteRef::new(0, 10), 0.0);
    block(&mut synth, 4, 100.0);
    assert_eq!((synth.active_voices(), synth.suspended_voices()), (0, 0));
    assert!(!synth.voices_mut().next().unwrap().sounding);

    // A new note takes a waiting voice over before it takes a heard one.
    synth.note_on(NoteRef::new(0, 20), 1.0);
    synth.note_on(NoteRef::new(0, 30), 0.5);
    block(&mut synth, 4, 100.0);
    block(&mut synth, FADE, 4.0);
    assert_eq!(synth.suspended_voices(), 1);
    synth.note_on(NoteRef::new(0, 40), 1.0);
    let (output, ..) = block(&mut synth, 4, 100.0);
    assert_eq!(output, [60.0; 4]);
}

#[test]
fn two_voices_of_nearly_the_same_loudness_do_not_take_turns() {
    let mut synth = levels(2, true);
    synth.note_on(NoteRef::new(0, 10), 0.50);
    synth.note_on(NoteRef::new(0, 20), 0.49);
    block(&mut synth, FADE, 4.0);
    for round in 0..20 {
        for (index, voice) in synth.voices_mut().enumerate() {
            // Which is the louder changes every block, by a little.
            voice.audible = if (index + round) % 2 == 0 { 0.55 } else { 0.5 };
        }
        let (output, ..) = block(&mut synth, FADE, 4.0);
        assert_eq!(output, [10.0; FADE], "round {round}");
    }
    // A voice much louder does take the other's place.
    synth.voices_mut().nth(1).unwrap().audible = 1.0;
    let (output, ..) = block(&mut synth, 2 * FADE, 4.0);
    assert_eq!(output[2 * FADE - 1], 20.0);
}

#[test]
fn instruments_share_one_allowance_in_the_order_they_spend_it() {
    let (mut first, mut second) = (levels(2, true), levels(2, true));
    for synth in [&mut first, &mut second] {
        synth.note_on(NoteRef::new(0, 10), 1.0);
        synth.note_on(NoteRef::new(0, 20), 1.0);
    }
    let mut allowance = Allowance::new(10.0);
    let mut buffer = AudioBuffer::<f32>::new(1, 16);
    first.process_budgeted(&mut buffer, &ProcessContext::empty(), &mut allowance);
    second.process_budgeted(&mut buffer, &ProcessContext::empty(), &mut allowance);
    assert_eq!((first.demoted_voices(), first.active_voices()), (0, 2));
    // Two units are left: two demoted voices.
    assert_eq!((second.demoted_voices(), second.active_voices()), (2, 2));
    assert_eq!((allowance.spent(), allowance.limited()), (10.0, true));
}

#[test]
fn process_with_brings_thinned_voices_back_to_full_quality() {
    let mut synth = levels(3, true);
    for key in [10, 20, 30] {
        synth.note_on(NoteRef::new(0, key), 1.0);
    }
    block(&mut synth, FADE, 100.0);
    block(&mut synth, FADE, 5.0);
    assert_eq!((synth.demoted_voices(), synth.suspended_voices()), (1, 1));
    let mut buffer = AudioBuffer::<f32>::new(1, FADE + 8);
    synth.process(&mut buffer);
    assert_eq!(buffer.channel(0)[FADE..], [60.0; 8]);
    assert!(buffer.channel(0)[0] < 51.0, "faded back in");
    assert_eq!((synth.demoted_voices(), synth.suspended_voices()), (0, 0));
    assert_eq!(tiers(&mut synth), [0, 0, 0]);
}

/// The largest step between neighbouring samples.
fn largest_jump(signal: &[f32]) -> f32 {
    signal
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0, f32::max)
}

#[test]
fn the_organ_is_demoted_cut_and_brought_back_with_no_click() {
    // Low notes, whose own sound is smooth, and no key click.
    let params = OrganParams {
        click: 0.0,
        ..OrganParams::full()
    };
    let make = || {
        let mut synth = Synth::new(OrganVoice::voices(3, params));
        synth.prepare(&spec(BLOCK));
        synth
    };
    let script = [(0, on(36)), (0, on(43)), (0, on(48))];
    let (full, clean, thin) = (
        OrganVoice::FULL_COST,
        OrganVoice::CLEAN_COST,
        OrganVoice::THIN_COST,
    );
    let steps = [
        3.0 * full,
        3.0 * clean,
        3.0 * thin,
        thin,
        0.0,
        2.0 * thin,
        full + 2.0 * clean,
        3.0 * full,
    ];
    const STEP: usize = 4_096;
    let frames = steps.len() * STEP;
    let free = play(&mut make(), &script, frames, BLOCK, |_| How::Free);
    let mut synth = make();
    let governed = play(&mut synth, &script, frames, BLOCK, |at| {
        How::Within(steps[at / STEP])
    });
    // Once the notes have started.
    let steady = largest_jump(&free[2_000..]);
    let worst = largest_jump(&governed[2_000..]);
    assert!(worst <= 1.05 * steady, "{worst} against {steady}");
    // The budgets did what they say: silent with none, all voices back, and
    // thinner in between.
    let rms = |from: usize| {
        let part = &governed[from * STEP + STEP / 2..(from + 1) * STEP];
        (part.iter().map(|s| s * s).sum::<f32>() / part.len() as f32).sqrt()
    };
    assert_eq!(rms(4), 0.0);
    assert!(rms(3) < 0.8 * rms(2), "one voice of three");
    assert!(rms(2) < rms(0), "three of nine drawbars");
    // All three notes again, whose wheels, having stood still, no longer
    // add up as they did.
    assert!(rms(7) > rms(5) && rms(5) > 0.0);
    assert_eq!((synth.demoted_voices(), synth.suspended_voices()), (0, 0));
    // A cut with no fade would have been heard: the sound is far from
    // silence where it is cut.
    let peak = free.iter().fold(0.0f32, |most, s| most.max(s.abs()));
    // Far above the steps allowed: a voice cut with no fade would show.
    assert!(peak > 10.0 * steady, "{peak} against {steady}");
}

#[test]
fn the_thinnest_organ_tier_keeps_the_three_loudest_drawbars() {
    // 16' and 8' all the way, 4' next, and the others softer.
    let params = OrganParams {
        click: 0.0,
        leakage: 0.0,
        ..OrganParams::registration("838265432")
    };
    let render = |registration: Option<OrganParams>, units: f64| {
        let mut synth = Synth::new(OrganVoice::voices(1, registration.unwrap_or(params)));
        synth.prepare(&spec(BLOCK));
        play(&mut synth, &[(0, on(60))], 4_800, BLOCK, |_| {
            How::Within(units)
        })
    };
    let thin = render(None, OrganVoice::THIN_COST);
    // The same wheels as a registration with those three alone: 16' at 8,
    // 8' at 8 and 2 2/3' at 6 being the loudest.
    let alone = OrganParams {
        click: 0.0,
        leakage: 0.0,
        ..OrganParams::registration("808060000")
    };
    let expected = render(Some(alone), f64::INFINITY);
    assert_eq!(thin, expected);
}

#[test]
fn a_demoted_organ_voice_loses_its_leakage_and_noise() {
    let render = |leakage: f64, units: f64| {
        let params = OrganParams {
            click: 0.0,
            leakage,
            ..OrganParams::registration("888000000")
        };
        let mut synth = Synth::new(OrganVoice::voices(1, params));
        synth.prepare(&spec(BLOCK));
        play(&mut synth, &[(0, on(60))], 4_800, BLOCK, |_| {
            How::Within(units)
        })
    };
    let clean = render(1.0, OrganVoice::CLEAN_COST);
    assert_eq!(clean, render(0.0, f64::INFINITY));
    assert_ne!(clean, render(1.0, f64::INFINITY));
}

#[test]
fn a_demoted_organ_voice_has_no_click_noise_and_a_resumed_one_no_percussion() {
    let silent_drawbars = |click: f64, percussion: bool| {
        let params = OrganParams {
            click,
            leakage: 0.0,
            percussion: OrganParams::jazz().percussion.filter(|_| percussion),
            ..OrganParams::registration("000000000")
        };
        let mut synth = Synth::new(OrganVoice::voices(1, params));
        synth.prepare(&spec(BLOCK));
        synth
    };
    let heard = |signal: &[f32]| signal.iter().any(|&sample| sample != 0.0);
    let note = [(0, on(60))];
    // With every drawbar in, the click's noise is all there is on tier 0.
    let render = |units: f64| {
        let mut synth = silent_drawbars(0.8, false);
        play(&mut synth, &note, 2_048, BLOCK, |_| How::Within(units))
    };
    assert!(heard(&render(OrganVoice::FULL_COST)));
    assert!(!heard(&render(OrganVoice::CLEAN_COST)));

    // And the percussion is, which a voice that waited does not take up
    // again.
    let mut synth = silent_drawbars(0.0, true);
    assert!(heard(&play(&mut synth, &note, 512, BLOCK, |_| How::Free)));
    play(&mut synth, &[], 512, BLOCK, |_| How::Within(0.0));
    assert_eq!(synth.suspended_voices(), 1);
    assert!(!heard(&play(&mut synth, &[], 2_048, BLOCK, |_| How::Free)));
    assert_eq!(synth.active_voices(), 1);
}
