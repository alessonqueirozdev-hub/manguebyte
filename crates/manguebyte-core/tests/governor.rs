//! Tests of `manguebyte_core::governor` on a fake clock, so that every
//! figure is the same on every machine.
//!
//! The synthetic instrument costs `overhead + per_unit * spent` of a
//! block's deadline, and its clock is moved by that much each block. Its
//! loads are fractions of the deadline; the defaults aim at 70 %, settle
//! at 70 % less the 5 % hysteresis (66.5 %) and answer anything above the
//! 90 % ceiling in one block.

use std::time::Duration;

use manguebyte_core::governor::{Allowance, Clock, Governor, GovernorConfig};

const RATE: f64 = 48_000.0;
const FRAMES: usize = 128;
const TARGET: f64 = 0.7;

/// A clock moved by hand.
#[derive(Debug, Default)]
struct FakeClock {
    now: Duration,
    reads: u64,
}

impl Clock for FakeClock {
    fn now(&mut self) -> Duration {
        self.reads += 1;
        self.now
    }
}

fn deadline(frames: usize) -> f64 {
    frames as f64 / RATE
}

/// An instrument asked for `demand` units, each costing `per_unit` of a
/// deadline, plus `overhead` of it whatever it renders. It spends whole
/// units, never more than its budget.
#[derive(Debug, Clone, Copy)]
struct Instrument {
    demand: f64,
    per_unit: f64,
    overhead: f64,
}

impl Instrument {
    /// The units that fill a whole deadline.
    fn capacity(&self) -> f64 {
        (1.0 - self.overhead) / self.per_unit
    }
}

struct Rig {
    governor: Governor<FakeClock>,
    /// Multiplies the next block's cost, for spikes and noise.
    factor: f64,
}

impl Rig {
    fn new(config: GovernorConfig) -> Rig {
        let mut governor = Governor::with_clock(config, FakeClock::default());
        governor.set_sample_rate(RATE);
        Rig {
            governor,
            factor: 1.0,
        }
    }

    /// Renders one block and returns (budget given, units spent, load).
    fn block(&mut self, instrument: Instrument, frames: usize) -> (f64, f64, f64) {
        let budget = self.governor.budget();
        let spent = instrument.demand.min(budget.floor());
        let load = (instrument.overhead + instrument.per_unit * spent) * self.factor;
        self.governor.begin_block();
        self.governor.clock_mut().now += Duration::from_secs_f64(load * deadline(frames));
        self.governor.end_block(frames, spent);
        (budget, spent, load)
    }

    fn run(&mut self, instrument: Instrument, blocks: usize) -> Vec<(f64, f64, f64)> {
        (0..blocks)
            .map(|_| self.block(instrument, FRAMES))
            .collect()
    }
}

/// An organ asked for twice what fills the deadline.
const OVERLOAD: Instrument = Instrument {
    demand: 2_000.0,
    per_unit: 0.000_95,
    overhead: 0.05,
};

fn blocks_in(seconds: f64) -> usize {
    (seconds / deadline(FRAMES)).ceil() as usize
}

#[test]
fn a_step_overload_is_brought_under_the_target_and_kept_there() {
    let mut rig = Rig::new(GovernorConfig::default());
    let quiet = Instrument {
        demand: 400.0,
        ..OVERLOAD
    };
    assert!(OVERLOAD.demand > 1.9 * OVERLOAD.capacity());
    // Quiet playing never limits anything.
    for (budget, spent, load) in rig.run(quiet, 200) {
        assert!(budget >= quiet.demand && spent == quiet.demand && load < TARGET);
    }
    let blocks = rig.run(OVERLOAD, blocks_in(5.0));
    // The step itself: the governor had learnt the cost at the quiet
    // level, so even the first block is not late.
    assert!(blocks[0].2 < 1.0, "first block's load {}", blocks[0].2);
    let under = blocks
        .iter()
        .position(|&(.., load)| load <= TARGET)
        .unwrap();
    assert!(under <= 3, "took {under} blocks to get under the target");
    for (index, &(budget, spent, load)) in blocks.iter().enumerate().skip(under) {
        assert!(load <= TARGET, "block {index}: load {load}");
        assert!(
            spent >= budget - 1.0,
            "the instrument is held by its budget"
        );
    }
    // It degrades only as far as needed: the last load is within the
    // hysteresis band below the target.
    let last = blocks.last().unwrap().2;
    assert!(last > TARGET * 0.9, "settled at {last}, needlessly low");
}

#[test]
fn a_first_overload_with_nothing_learnt_is_answered_in_one_block() {
    let mut rig = Rig::new(GovernorConfig::default());
    let blocks = rig.run(OVERLOAD, blocks_in(2.0));
    assert!(blocks[0].0.is_infinite(), "nothing learnt, no limit");
    assert!(blocks[0].2 > 1.8);
    // Above the ceiling, the whole measurement is taken at once.
    assert!(blocks[1].2 <= TARGET, "second block's load {}", blocks[1].2);
    assert!(blocks[1..].iter().all(|&(.., load)| load <= TARGET));
}

#[test]
fn a_rise_between_the_target_and_the_ceiling_is_answered_within_four_blocks() {
    let mut rig = Rig::new(GovernorConfig::default());
    rig.run(OVERLOAD, blocks_in(1.0));
    let settled = rig.run(OVERLOAD, 1)[0].2;
    // The machine gets 25 % slower: the load lands between target and ceiling.
    rig.factor = 1.25;
    let blocks = rig.run(OVERLOAD, blocks_in(2.0));
    assert!(blocks[0].2 > TARGET && blocks[0].2 < 0.9, "{}", blocks[0].2);
    let under = blocks
        .iter()
        .position(|&(.., load)| load <= TARGET)
        .unwrap();
    assert!(under <= 4, "took {under} blocks, settled at {settled}");
    assert!(blocks[under..].iter().all(|&(.., load)| load <= TARGET));
}

#[test]
fn a_slow_ramp_of_cost_is_followed_without_passing_the_target() {
    let mut rig = Rig::new(GovernorConfig::default());
    rig.run(OVERLOAD, blocks_in(1.0));
    // The cost doubles over ten seconds.
    let ramp = blocks_in(10.0);
    let mut worst: f64 = 0.0;
    for index in 0..ramp {
        rig.factor = 1.0 + index as f64 / ramp as f64;
        worst = worst.max(rig.block(OVERLOAD, FRAMES).2);
    }
    assert!(worst <= TARGET * 1.01, "worst load {worst} on the ramp");
}

#[test]
fn recovery_is_monotonic_and_rate_limited() {
    let config = GovernorConfig::default();
    let mut rig = Rig::new(config);
    // A spike: one block takes three times as long.
    rig.run(OVERLOAD, blocks_in(1.0));
    let before = rig.governor.budget();
    rig.factor = 3.0;
    rig.block(OVERLOAD, FRAMES);
    rig.factor = 1.0;
    let cut = rig.governor.budget();
    assert!(cut < before / 2.5, "a spike above the ceiling cuts at once");

    // At most doubling every half second, at 128 frames a block.
    let growth = (deadline(FRAMES) / 0.5).exp2();
    let mut budgets = vec![cut];
    budgets.extend(
        rig.run(OVERLOAD, blocks_in(8.0))
            .iter()
            .skip(1)
            .map(|b| b.0),
    );
    budgets.push(rig.governor.budget());
    for pair in budgets.windows(2) {
        assert!(
            pair[1] >= pair[0],
            "the budget turned down while recovering"
        );
        assert!(
            pair[1] <= pair[0] * growth * (1.0 + 1e-12),
            "rose from {} to {}, more than {growth}",
            pair[0],
            pair[1]
        );
    }
    // And it gets back to where it was, give or take the hysteresis: the
    // budget before the spike sat at the top of the band.
    let end = *budgets.last().unwrap();
    assert!(end >= before * 0.9, "recovered to {end} of {before}");
    // The doubling time bounds it from below: it cannot be back in less.
    let three_times = (3.0f64).log2() * 0.5;
    let back = budgets.iter().position(|&b| b >= before * 0.9).unwrap();
    assert!(back as f64 * deadline(FRAMES) >= three_times * 0.8);
}

#[test]
fn recovery_rate_does_not_depend_on_the_block_size() {
    // The same second of audio in blocks of 32 and of 512: the budget may
    // grow by the same factor.
    for frames in [32, 512] {
        let mut rig = Rig::new(GovernorConfig::default());
        for _ in 0..(RATE as usize / frames) {
            rig.block(OVERLOAD, frames);
        }
        rig.factor = 3.0;
        rig.block(OVERLOAD, frames);
        rig.factor = 1.0;
        let cut = rig.governor.budget();
        for _ in 0..(RATE as usize / 2 / frames) {
            rig.block(OVERLOAD, frames);
        }
        let ratio = rig.governor.budget() / cut;
        assert!(
            ratio <= 2.0 * 1.001,
            "{frames} frames: grew {ratio} in 0.5 s"
        );
        assert!(ratio > 1.3, "{frames} frames: grew only {ratio} in 0.5 s");
    }
}

/// A deterministic noise from -1 to 1.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// Runs `seconds` with each block's cost moved by up to `noise` either way,
/// after a second to settle, and returns the budgets given.
fn noisy(noise: f64, seconds: f64) -> Vec<f64> {
    let mut rig = Rig::new(GovernorConfig::default());
    let mut random = Noise(7);
    let mut run = |rig: &mut Rig, blocks| {
        (0..blocks)
            .map(|_| {
                rig.factor = 1.0 + noise * random.next();
                rig.block(OVERLOAD, FRAMES).0
            })
            .collect::<Vec<_>>()
    };
    run(&mut rig, blocks_in(1.0));
    run(&mut rig, blocks_in(seconds))
}

fn reversals(budgets: &[f64]) -> usize {
    let mut last = 0.0f64;
    let mut count = 0;
    for pair in budgets.windows(2) {
        let step = pair[1] - pair[0];
        if step != 0.0 {
            if step.signum() == -last.signum() {
                count += 1;
            }
            last = step;
        }
    }
    count
}

#[test]
fn noise_inside_the_hysteresis_moves_nothing() {
    // ±2 % on every block, inside the band of 5 %.
    let budgets = noisy(0.02, 10.0);
    let first = budgets[0];
    assert!(
        budgets.iter().all(|&b| b == first),
        "the budget moved under small noise"
    );
}

#[test]
fn noise_wider_than_the_band_settles_below_its_peaks() {
    // ±20 % on every block, four times the band, moves nothing either once
    // settled: the estimate follows the noise's peaks, and the budget sits
    // where the costliest blocks fit.
    let budgets = noisy(0.20, 60.0);
    assert!(budgets.iter().all(|&b| b == budgets[0]));
    assert!(budgets[0] < noisy(0.02, 1.0)[0] * 0.95);
}

#[test]
fn large_noise_moves_the_budget_within_a_stated_bound() {
    // ±50 % on every block. Measured on this rig: the budget stays between
    // 3.6 % under and 9.7 % over its mean, and turns round 191 times in a
    // minute, about three a second, in small steps; the bounds below leave
    // a little room.
    let budgets = noisy(0.50, 60.0);
    let mean = budgets.iter().sum::<f64>() / budgets.len() as f64;
    let (low, high) = budgets.iter().fold((f64::MAX, 0.0f64), |(low, high), &b| {
        (low.min(b), high.max(b))
    });
    assert!(
        low >= mean * 0.95 && high <= mean * 1.12,
        "{low} to {high} around {mean}"
    );
    let turns = reversals(&budgets);
    assert!(turns <= 240, "turned round {turns} times in a minute");
}

#[test]
fn offline_gives_the_full_budget_and_never_reads_the_clock() {
    let mut rig = Rig::new(GovernorConfig::default());
    rig.governor.set_max_budget(5_000.0);
    rig.run(OVERLOAD, 50);
    assert!(rig.governor.budget() < 1_000.0);
    let reads = rig.governor.clock().reads;
    let stats = rig.governor.stats();

    rig.governor.set_offline(true);
    rig.governor.set_quality_floor(10.0);
    for factor in [1.0, 10.0, 100.0] {
        rig.factor = factor;
        for (budget, ..) in rig.run(OVERLOAD, 100) {
            assert_eq!(budget, 5_000.0);
        }
        rig.governor.note_missed(3);
        rig.governor.record(Duration::from_secs(1), FRAMES, 1.0);
    }
    assert_eq!(rig.governor.clock().reads, reads, "offline read the clock");
    assert_eq!(rig.governor.stats(), stats, "offline learnt something");
    assert!(!rig.governor.is_limiting());

    // Back online it picks up where it was.
    rig.governor.set_offline(false);
    rig.factor = 1.0;
    assert!(rig.governor.budget() < 1_000.0);
}

#[test]
fn the_quality_floor_holds_whatever_the_load() {
    let mut rig = Rig::new(GovernorConfig::default());
    rig.governor.set_quality_floor(900.0);
    let blocks = rig.run(OVERLOAD, 500);
    assert!(blocks[1..].iter().all(|&(budget, ..)| budget == 900.0));
    // The floor costs the deadline, as asked: 0.05 + 900 * 0.00095.
    assert!(blocks.last().unwrap().2 > TARGET);
    // Lowering it lets the governor cut.
    rig.governor.set_quality_floor(0.0);
    rig.run(OVERLOAD, 3);
    assert!(rig.governor.budget() < 700.0);
    assert!(rig.governor.last_load() <= TARGET);
}

#[test]
fn the_budget_stays_within_its_maximum() {
    let mut rig = Rig::new(GovernorConfig::default());
    rig.governor.set_max_budget(100.0);
    let light = Instrument {
        demand: 100.0,
        ..OVERLOAD
    };
    for (budget, ..) in rig.run(light, 2_000) {
        assert_eq!(budget, 100.0);
    }
    assert!(!rig.governor.is_limiting());
}

#[test]
fn missed_jobs_count_as_an_overload() {
    let mut rig = Rig::new(GovernorConfig::default());
    rig.run(OVERLOAD, blocks_in(1.0));
    let before = rig.governor.budget();
    // The clock says the block fitted, but the pool dropped jobs.
    rig.governor.begin_block();
    rig.governor.clock_mut().now += Duration::from_secs_f64(0.5 * deadline(FRAMES));
    rig.governor.note_missed(2);
    rig.governor.end_block(FRAMES, before.floor());
    assert_eq!(rig.governor.last_load(), 1.0);
    assert_eq!(rig.governor.stats().missed_blocks, 1);
    assert!(
        rig.governor.budget() <= before * 0.75,
        "{} of {before}",
        rig.governor.budget()
    );
    // The count is for one block only.
    rig.block(OVERLOAD, FRAMES);
    assert!(rig.governor.last_load() < 1.0);
}

#[test]
fn a_quiet_or_silent_stretch_keeps_what_was_learnt() {
    let mut rig = Rig::new(GovernorConfig::default());
    rig.run(OVERLOAD, blocks_in(1.0));
    let learnt = rig.governor.budget();
    let unit = rig.governor.unit_load();
    // A few quiet notes, where the fixed work outweighs the units, then
    // silence.
    let few = Instrument {
        demand: 4.0,
        ..OVERLOAD
    };
    let silent = Instrument {
        demand: 0.0,
        ..OVERLOAD
    };
    rig.run(few, blocks_in(3.0));
    rig.run(silent, blocks_in(3.0));
    assert_eq!(rig.governor.unit_load(), unit);
    assert_eq!(rig.governor.budget(), learnt);
    // So the full chord after the rest is not late.
    assert!(rig.block(OVERLOAD, FRAMES).2 <= TARGET);
}

#[test]
fn a_cost_estimated_too_high_is_corrected_while_the_budget_holds_the_instrument() {
    // Learnt at a time when everything took three times as long, then the
    // load is low because the budget is: the governor still learns from
    // those blocks, as the instrument spends its whole budget in them.
    let mut rig = Rig::new(GovernorConfig::default());
    rig.factor = 3.0;
    rig.run(OVERLOAD, 20);
    rig.factor = 1.0;
    let low = rig.governor.budget();
    rig.run(OVERLOAD, blocks_in(5.0));
    assert!(rig.governor.budget() > low * 2.5);
    assert!(rig.governor.last_load() > TARGET * 0.9);
}

#[test]
fn allowance_spends_in_priority_order() {
    let mut allowance = Allowance::new(110.0);
    let tiers = [32.0, 8.0];
    let mut given = Vec::new();
    for _voice in 0..6 {
        let tier = tiers.iter().position(|&cost| allowance.try_take(cost));
        given.push(tier);
    }
    // Three voices at the full tier, then the cheap one, then nothing.
    assert_eq!(given, [Some(0), Some(0), Some(0), Some(1), None, None]);
    assert_eq!(allowance.spent(), 104.0);
    assert_eq!(allowance.remaining(), 6.0);
    assert_eq!(allowance.take_up_to(10.0), 6.0);
    allowance.force(5.0);
    assert_eq!(allowance.remaining(), 0.0);
    assert_eq!(allowance.spent(), 115.0);
    // An unlimited budget gives everything.
    let mut unlimited = Allowance::new(f64::INFINITY);
    assert!(unlimited.try_take(1e9));
    assert_eq!(unlimited.take_up_to(7.0), 7.0);
}

#[test]
fn nonsense_settings_are_clamped() {
    let config = GovernorConfig::default()
        .target(5.0)
        .ceiling(0.1)
        .hysteresis(-1.0)
        .attack(f64::NAN)
        .learn_above(2.0);
    let governor = Governor::with_clock(config, FakeClock::default());
    let checked = governor.config();
    assert_eq!(
        checked,
        GovernorConfig::default()
            .target(1.0)
            .ceiling(1.0)
            .hysteresis(0.0)
            .learn_above(1.0)
    );
    let mut governor = governor;
    governor.set_sample_rate(-1.0);
    assert_eq!(governor.sample_rate(), 48_000.0);
    governor.set_quality_floor(f64::NAN);
    assert_eq!(governor.quality_floor(), 0.0);
    // A block of no frames, and an end without a beginning, are ignored.
    governor.record(Duration::from_secs(1), 0, 1.0);
    governor.end_block(128, 1.0);
    assert_eq!(governor.stats().blocks, 0);
}

#[test]
fn the_real_clock_reads_forward() {
    let mut governor = Governor::new(GovernorConfig::default());
    governor.begin_block();
    let mut sum = 0.0f64;
    for index in 0..10_000 {
        sum += f64::from(index).sqrt();
    }
    std::hint::black_box(sum);
    governor.end_block(FRAMES, 1.0);
    assert_eq!(governor.stats().blocks, 1);
    assert!(governor.last_load() >= 0.0);
}
