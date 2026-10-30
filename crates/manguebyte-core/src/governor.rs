//! A governor that keeps an instrument's rendering inside the block's time
//! by giving it a budget to spend (ADR 0018).
//!
//! An instrument can be asked for more than the machine can render: a full
//! organ with every coupler drawn on a weak laptop. Rather than miss the
//! deadline and glitch, it can render less: demote voices to a cheaper
//! tier, drop the quietest partials, steal the least audible voices. The
//! [`Governor`] tells it how much it may render. Each block it measures
//! the time the rendering took against the block's deadline (its frames
//! over the sample rate), and gives a budget for the next block in
//! abstract *cost units*, which the instrument spends in its own order of
//! priority.
//!
//! # The cost model
//!
//! The instrument says what things cost relative to each other: a partial
//! of a pipe costs 1, a voice at its full tier 32, at its cheap tier 8,
//! whatever its kernels make true. It reports how many units it spent in
//! each block. The governor learns the one thing that depends on the
//! machine, the *load per unit*: the fraction of a deadline one unit takes,
//! measured as the block's load over the units spent. The budget is then
//! the number of units that the estimate says fit under the target load.
//! Units are per block, whatever its length: a voice costs the same number
//! of units in a block of 64 frames as in one of 512, since its time and the
//! deadline both grow with the frames.
//!
//! Work that does not grow with the units (the mix, an effect after the
//! voices) is not modelled. It makes the estimate too high when few units
//! are spent, which errs on the safe side, and does not move where the
//! governor settles when the instrument is held by its budget: the
//! estimate and the budget meet where the measured load is what the
//! governor aims at. ADR 0018 works this out.
//!
//! # How it moves
//!
//! - **Attack.** When a block's load per unit is above the estimate, the
//!   estimate moves towards it by [`attack`](GovernorConfig::attack) of the
//!   way at once (half, unless set), and all the way when the block's load
//!   is above the [ceiling](GovernorConfig::ceiling). The budget follows
//!   down in the same block. One block raises the estimate by at most
//!   [`max_rise`](GovernorConfig::max_rise) (a factor of two unless set),
//!   so a thread preempted for tens of milliseconds halves the budget
//!   rather than silence the instrument. The first block learnt from is
//!   taken as it is.
//! - **Spikes.** By default every block is believed.
//!   [`spike_blocks`](GovernorConfig::spike_blocks) makes the estimate go
//!   by the lowest load per unit of the last few blocks learnt from, so a
//!   rise moves it only once it has lasted that many: a lone preemption
//!   then moves nothing, but a cost that really varies from block to block
//!   makes more blocks late. On a laptop busy with other programs, with a
//!   synthetic organ asked for twice what fits, believing every block left
//!   0.3 to 0.7 % of the blocks late and rendered 0.3 to 6 % of the organ;
//!   going by the last three, 6 to 39 % late and 7 to 18 % rendered
//!   (ADR 0018). Which is better is the instrument's choice.
//! - **Release.** When it is below, the estimate falls with the
//!   [release](GovernorConfig::release) time constant, and the budget rises
//!   by at most a factor of two every [`doubling`](GovernorConfig::doubling)
//!   time. Recovery is slow and steady: the budget never rises by more than
//!   that rate, and while the load stays low it never turns back down. In
//!   the defaults, a budget that was halved is back to 90 % of where it was
//!   within three seconds.
//! - **Hysteresis.** The budget aims a fraction
//!   [`hysteresis`](GovernorConfig::hysteresis) below what the estimate
//!   says reaches the target, falls only when the aim drops that fraction
//!   below it or the load, read through the spike rule, passes the target,
//!   and starts rising only when the aim is that fraction above it. Noise
//!   smaller than the band moves nothing.
//! - **Learning.** Only blocks that say something about the cost are
//!   learnt from: ones whose load is at least half the target (unless set),
//!   or in which the instrument was *limited*, wanting more than its
//!   budget. A quiet block, in which fixed work outweighs the units, leaves
//!   the estimate as it was; so does a silent one. The estimate is kept
//!   while the instrument is quiet, so the first chord after a rest gets
//!   the budget the last one had, rather than everything at once. A
//!   limited block in which nothing could be spent, the budget being below
//!   the cheapest thing the instrument renders, lets the estimate fall at
//!   the release rate, so the budget climbs back rather than stay stuck.
//!
//! Until the first block worth learning from, the budget is the
//! [maximum](Governor::set_max_budget) (unlimited unless set).
//!
//! # Offline
//!
//! [`set_offline`](Governor::set_offline) turns the governor off: the budget
//! is always the maximum, the clock is never read, and nothing is learnt, so
//! an offline render is at full quality and the same on every machine. Turn
//! it on for a host's offline bounce (CLAP's `render` extension, VST 3's
//! offline process mode).
//!
//! # The floor
//!
//! [`set_quality_floor`](Governor::set_quality_floor) sets the fewest units
//! the budget ever gives, overload or not: a plugin's promise that a sound
//! never thins below a certain point, at the cost of the deadline.
//!
//! # A render pool
//!
//! A block in which a render pool (ADR 0015) missed jobs, because its wait
//! limit passed while a worker still held them, counts as at least a whole
//! deadline's load, whatever the clock said, and goes through the same
//! rules as any other load (with
//! [`spike_blocks`](GovernorConfig::spike_blocks) above one, it must
//! last). Tell the governor through [`note_missed`](Governor::note_missed)
//! before [`end_block`](Governor::end_block), and time the pool's `run` and
//! the mix after it between [`begin_block`](Governor::begin_block) and
//! `end_block`.
//!
//! # Real time
//!
//! The governor holds a handful of numbers. Nothing it does allocates,
//! locks or waits; [`end_block`](Governor::end_block) works out one `exp`
//! and one `exp2`. With [`SystemClock`] it reads the monotonic clock twice
//! a block, through [`Instant::now`]: `QueryPerformanceCounter` on Windows,
//! which reads the processor's time-stamp counter without entering the
//! kernel where the counter is invariant (all current x86 and ARM
//! machines), `clock_gettime` through the vDSO on Linux and Android, which
//! makes no system call while the kernel's clock source can be read from
//! user space (not on some virtual machines), and `clock_gettime` with
//! `CLOCK_UPTIME_RAW` on macOS and iOS, which reads a page the kernel
//! shares. A test gives it a clock of its own ([`Clock`]).
//!
//! ```
//! use std::time::Duration;
//! use manguebyte_core::governor::{Allowance, Clock, Governor, GovernorConfig};
//!
//! /// A clock the example moves by hand.
//! struct Fake(Duration);
//! impl Clock for Fake {
//!     fn now(&mut self) -> Duration {
//!         self.0
//!     }
//! }
//!
//! let mut governor = Governor::with_clock(GovernorConfig::default(), Fake(Duration::ZERO));
//! governor.set_sample_rate(48_000.0);
//! // 64 voices of 4 units each are asked for; one unit takes 1/128 of a
//! // block's time, so 128 units fill the deadline.
//! let unit = Duration::from_secs_f64(128.0 / 48_000.0 / 128.0);
//! for _ in 0..50 {
//!     let mut allowance = Allowance::new(governor.budget());
//!     governor.begin_block();
//!     let voices = (0..64).filter(|_| allowance.try_take(4.0)).count();
//!     governor.clock_mut().0 += unit * (4 * voices) as u32;
//!     governor.end_block(128, allowance.spent(), allowance.limited());
//! }
//! // It renders as many voices as keep the load under the target of 70 %.
//! assert!(governor.last_load() <= 0.7);
//! assert!(governor.budget() >= 80.0);
//! ```

use std::time::{Duration, Instant};

/// A monotonic source of time for a [`Governor`]: the time since some
/// fixed moment of its own choosing.
///
/// [`SystemClock`] is the real one; tests give a fake one that they move
/// by hand, so their results do not depend on the machine.
pub trait Clock {
    /// The time now. It must never go backwards.
    fn now(&mut self) -> Duration;
}

/// The system's monotonic clock, through [`Instant`]. See the
/// [module documentation](self#real-time) for what reading it costs.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// A clock counting from now.
    pub fn new() -> SystemClock {
        SystemClock {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> SystemClock {
        SystemClock::new()
    }
}

impl Clock for SystemClock {
    fn now(&mut self) -> Duration {
        self.origin.elapsed()
    }
}

/// How a [`Governor`] reacts. Every value is a plain number, checked and
/// clamped into its range when the governor is made.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GovernorConfig {
    target: f64,
    ceiling: f64,
    hysteresis: f64,
    attack: f64,
    release: Duration,
    doubling: Duration,
    learn_above: f64,
    max_rise: f64,
    spike_blocks: usize,
}

/// The most blocks [`GovernorConfig::spike_blocks`] can ask for.
pub const MAX_SPIKE_BLOCKS: usize = 16;

impl GovernorConfig {
    /// The defaults: a target load of 70 % of the deadline, a ceiling of
    /// 90 %, a hysteresis of 5 %, an attack of half the way a block, a
    /// release time constant of half a second, a budget that at most
    /// doubles every half second, learning from blocks loaded to at least
    /// half the target, an estimate that one block at most doubles, and
    /// every block believed (see [`spike_blocks`](Self::spike_blocks)).
    pub const fn new() -> GovernorConfig {
        GovernorConfig {
            target: 0.7,
            ceiling: 0.9,
            hysteresis: 0.05,
            attack: 0.5,
            release: Duration::from_millis(500),
            doubling: Duration::from_millis(500),
            learn_above: 0.5,
            max_rise: 2.0,
            spike_blocks: 1,
        }
    }

    /// The load, as a fraction of the deadline, the governor keeps the
    /// rendering at or under (0.01 to 1). It settles a little below it, by
    /// the [hysteresis](Self::hysteresis).
    pub fn target(mut self, target: f64) -> GovernorConfig {
        self.target = target;
        self
    }

    /// The load above which a block is answered in full at once, rather
    /// than by the attack (at least the target).
    pub fn ceiling(mut self, ceiling: f64) -> GovernorConfig {
        self.ceiling = ceiling;
        self
    }

    /// The width of the band, as a fraction of the budget, inside which
    /// the budget does not move (0 to 0.5).
    pub fn hysteresis(mut self, hysteresis: f64) -> GovernorConfig {
        self.hysteresis = hysteresis;
        self
    }

    /// How much of the way the cost estimate moves towards a higher
    /// measurement in one block, from just above 0 to 1 (at once).
    pub fn attack(mut self, attack: f64) -> GovernorConfig {
        self.attack = attack;
        self
    }

    /// The time constant with which the cost estimate falls towards a lower
    /// measurement.
    pub fn release(mut self, release: Duration) -> GovernorConfig {
        self.release = release;
        self
    }

    /// The shortest time in which a recovering budget can double.
    pub fn doubling(mut self, doubling: Duration) -> GovernorConfig {
        self.doubling = doubling;
        self
    }

    /// The fraction of the target load a block must reach to be learnt
    /// from, unless the instrument was limited in it (0 to 1).
    pub fn learn_above(mut self, fraction: f64) -> GovernorConfig {
        self.learn_above = fraction;
        self
    }

    /// The most one block can multiply the cost estimate by, once it has
    /// one (at least 1; unlimited with `f64::INFINITY`).
    pub fn max_rise(mut self, factor: f64) -> GovernorConfig {
        self.max_rise = factor;
        self
    }

    /// How many blocks learnt from in a row a rise in the cost per unit
    /// must last to move the estimate (1 to [`MAX_SPIKE_BLOCKS`]): the
    /// estimate goes by the lowest of the last this many. One believes
    /// every block.
    pub fn spike_blocks(mut self, blocks: usize) -> GovernorConfig {
        self.spike_blocks = blocks;
        self
    }

    fn checked(self) -> GovernorConfig {
        let finite = |value: f64, default: f64| if value.is_finite() { value } else { default };
        let defaults = GovernorConfig::new();
        let target = finite(self.target, defaults.target).clamp(0.01, 1.0);
        GovernorConfig {
            target,
            ceiling: finite(self.ceiling, defaults.ceiling).max(target),
            hysteresis: finite(self.hysteresis, defaults.hysteresis).clamp(0.0, 0.5),
            attack: finite(self.attack, defaults.attack).clamp(f64::MIN_POSITIVE, 1.0),
            release: self.release,
            doubling: self.doubling,
            learn_above: finite(self.learn_above, defaults.learn_above).clamp(0.0, 1.0),
            max_rise: if self.max_rise.is_nan() {
                defaults.max_rise
            } else {
                self.max_rise.max(1.0)
            },
            spike_blocks: self.spike_blocks.clamp(1, MAX_SPIKE_BLOCKS),
        }
    }
}

impl Default for GovernorConfig {
    fn default() -> GovernorConfig {
        GovernorConfig::new()
    }
}

/// Counts kept since a [`Governor`] was made or reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GovernorStats {
    /// Blocks measured (none while offline).
    pub blocks: u64,
    /// Blocks whose load was above the target.
    pub over_target: u64,
    /// Blocks whose load was above the whole deadline, missed jobs
    /// included: blocks that were, or would have been, late.
    pub over_deadline: u64,
    /// Blocks in which a render pool missed jobs.
    pub missed_blocks: u64,
    /// Times the budget was lowered.
    pub cuts: u64,
}

/// Measures each block against its deadline and gives the instrument a
/// budget in cost units for the next one. See the
/// [module documentation](self).
#[derive(Debug, Clone)]
pub struct Governor<C: Clock = SystemClock> {
    config: GovernorConfig,
    clock: C,
    sample_rate: f64,
    offline: bool,
    floor: f64,
    max: f64,
    /// The budget for the next block.
    budget: f64,
    /// The estimated load of one unit; 0 until something was learnt.
    unit_load: f64,
    /// Whether the budget is on its way up to the aim.
    rising: bool,
    /// The load per unit of the last blocks learnt from, a ring of
    /// `spike_blocks`.
    recent: [f64; MAX_SPIKE_BLOCKS],
    recent_next: usize,
    recent_len: usize,
    /// When the block being measured began, if `begin_block` was called.
    started: Option<Duration>,
    /// Jobs a pool missed in the block being measured.
    missed: usize,
    last_load: f64,
    smoothed_load: f64,
    stats: GovernorStats,
}

impl Governor<SystemClock> {
    /// A governor on the system's clock. Until
    /// [`set_sample_rate`](Self::set_sample_rate) is called it assumes
    /// 48 kHz.
    pub fn new(config: GovernorConfig) -> Governor<SystemClock> {
        Governor::with_clock(config, SystemClock::new())
    }
}

impl<C: Clock> Governor<C> {
    /// A governor on a clock of the caller's.
    pub fn with_clock(config: GovernorConfig, clock: C) -> Governor<C> {
        Governor {
            config: config.checked(),
            clock,
            sample_rate: 48_000.0,
            offline: false,
            floor: 0.0,
            max: f64::INFINITY,
            budget: f64::INFINITY,
            unit_load: 0.0,
            rising: false,
            recent: [0.0; MAX_SPIKE_BLOCKS],
            recent_next: 0,
            recent_len: 0,
            started: None,
            missed: 0,
            last_load: 0.0,
            smoothed_load: 0.0,
            stats: GovernorStats::default(),
        }
    }

    /// The configuration, as checked.
    pub fn config(&self) -> GovernorConfig {
        self.config
    }

    /// The load the governor keeps the rendering at or under, as a
    /// fraction of the deadline.
    pub fn target(&self) -> f64 {
        self.config.target
    }

    /// The clock.
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// The clock, to move a fake one.
    pub fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }

    /// Sets the sample rate the deadlines are worked out from. Keeps what
    /// was learnt: the load of a unit does not depend on it, as long as
    /// the instrument's work per frame does not.
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        if sample_rate.is_finite() && sample_rate > 0.0 {
            self.sample_rate = sample_rate;
        }
    }

    /// The sample rate the deadlines are worked out from.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// Turns offline rendering on or off. Offline, the budget is always the
    /// maximum, the clock is not read and nothing is learnt; turning it
    /// off again picks up where the governor was.
    pub fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
        self.started = None;
        self.missed = 0;
    }

    /// Whether the governor is offline.
    pub fn is_offline(&self) -> bool {
        self.offline
    }

    /// Sets the fewest units the budget gives (0 unless set), whatever the
    /// load. It takes effect at once; it is held below the maximum.
    pub fn set_quality_floor(&mut self, units: f64) {
        self.floor = if units.is_finite() {
            units.max(0.0)
        } else {
            0.0
        };
        self.budget = self.clamp(self.budget);
    }

    /// The fewest units the budget gives.
    pub fn quality_floor(&self) -> f64 {
        self.floor.min(self.max)
    }

    /// Sets the most units the budget gives: what the instrument costs at
    /// full quality, if it knows (unlimited unless set). It takes effect at
    /// once.
    pub fn set_max_budget(&mut self, units: f64) {
        self.max = if units.is_nan() {
            f64::INFINITY
        } else {
            units.max(0.0)
        };
        self.budget = self.clamp(self.budget);
    }

    /// The most units the budget gives.
    pub fn max_budget(&self) -> f64 {
        self.max
    }

    /// The units the instrument may spend in the next block. Offline it is
    /// the maximum. Spending more is allowed; the governor will learn the
    /// cost of it and cut the next budget.
    pub fn budget(&self) -> f64 {
        if self.offline { self.max } else { self.budget }
    }

    /// Whether the budget is below the maximum: the instrument is being
    /// asked to render less than it could.
    pub fn is_limiting(&self) -> bool {
        self.budget() < self.max
    }

    /// The load of the last block measured: its rendering time over its
    /// deadline, raised to at least 1 if a pool missed jobs in it.
    pub fn last_load(&self) -> f64 {
        self.last_load
    }

    /// The load, smoothed with the attack and release, for a meter.
    pub fn load(&self) -> f64 {
        self.smoothed_load
    }

    /// The estimated load of one unit, as a fraction of a deadline; 0
    /// until a block worth learning from was measured.
    pub fn unit_load(&self) -> f64 {
        self.unit_load
    }

    /// Counts kept since the governor was made or reset.
    pub fn stats(&self) -> GovernorStats {
        self.stats
    }

    /// Forgets what was learnt and the counts: the budget goes back to the
    /// maximum. Keeps the configuration, the sample rate, the floor, the
    /// maximum and whether it is offline.
    pub fn reset(&mut self) {
        self.budget = self.max;
        self.unit_load = 0.0;
        self.rising = false;
        self.recent_next = 0;
        self.recent_len = 0;
        self.started = None;
        self.missed = 0;
        self.last_load = 0.0;
        self.smoothed_load = 0.0;
        self.stats = GovernorStats::default();
    }

    /// Marks the start of a block's rendering: reads the clock once (not
    /// offline).
    pub fn begin_block(&mut self) {
        if !self.offline {
            self.started = Some(self.clock.now());
        }
    }

    /// Counts jobs a render pool missed in the block being measured. The
    /// block then counts as at least a whole deadline's load.
    pub fn note_missed(&mut self, missed: usize) {
        if !self.offline {
            self.missed = self.missed.saturating_add(missed);
        }
    }

    /// Marks the end of a block of `frames` frames in which the instrument
    /// spent `spent` units, and was `limited` if it wanted more than its
    /// budget ([`Allowance::limited`]): reads the clock once, and works out
    /// the next budget. Does nothing offline, or if
    /// [`begin_block`](Self::begin_block) was not called.
    pub fn end_block(&mut self, frames: usize, spent: f64, limited: bool) {
        if self.offline {
            return;
        }
        let Some(started) = self.started.take() else {
            return;
        };
        let elapsed = self.clock.now().saturating_sub(started);
        self.record(elapsed, frames, spent, limited);
    }

    /// Works out the next budget from a block the caller timed itself:
    /// `elapsed` to render `frames` frames, as for
    /// [`end_block`](Self::end_block). Does nothing offline, or for a block
    /// of no frames.
    pub fn record(&mut self, elapsed: Duration, frames: usize, spent: f64, limited: bool) {
        let missed = core::mem::take(&mut self.missed);
        if self.offline || frames == 0 {
            return;
        }
        let deadline = frames as f64 / self.sample_rate;
        let mut load = elapsed.as_secs_f64() / deadline;
        if missed > 0 {
            load = load.max(1.0);
            self.stats.missed_blocks += 1;
        }
        let spent = if spent.is_finite() {
            spent.max(0.0)
        } else {
            0.0
        };
        let config = self.config;

        self.stats.blocks += 1;
        self.stats.over_target += u64::from(load > config.target);
        self.stats.over_deadline += u64::from(load > 1.0);
        self.last_load = load;

        let release = coefficient(deadline, config.release);
        let towards = |from: f64, to: f64, attack: f64| {
            let amount = if to > from { attack } else { release };
            from + (to - from) * amount
        };
        let attack_for = |load: f64| {
            if load > config.ceiling {
                1.0
            } else {
                config.attack
            }
        };
        self.smoothed_load = towards(self.smoothed_load, load, attack_for(load));

        // What the decisions below go by: the block's load, or, for a block
        // learnt from, the load its units would have had at the lowest cost
        // per unit of the last `spike_blocks` such blocks (its own unless
        // set), so that a spike shorter than that moves nothing.
        let mut steady_load = load;
        if spent > 0.0 && (limited || load >= config.learn_above * config.target) {
            self.recent[self.recent_next] = load / spent;
            self.recent_next = (self.recent_next + 1) % config.spike_blocks;
            self.recent_len = (self.recent_len + 1).min(config.spike_blocks);
            let per_unit = self.recent[..self.recent_len]
                .iter()
                .fold(f64::INFINITY, |low, &x| low.min(x));
            steady_load = per_unit * spent;
            self.unit_load = if self.unit_load > 0.0 {
                let bounded = per_unit.min(self.unit_load * config.max_rise);
                towards(self.unit_load, bounded, attack_for(steady_load))
            } else {
                per_unit
            };
        } else if limited && load <= config.target {
            // Nothing fitted: say the cost is lower, slowly.
            self.unit_load *= 1.0 - release;
        }

        let hysteresis = config.hysteresis;
        let over = steady_load > config.target;
        let aim = if self.unit_load > 0.0 {
            self.clamp(config.target * (1.0 - hysteresis) / self.unit_load)
        } else {
            self.max
        };
        if aim < self.budget && (over || aim < self.budget * (1.0 - hysteresis)) {
            self.budget = aim;
            self.rising = false;
            self.stats.cuts += 1;
        } else if over || aim <= self.budget {
            self.rising = false;
        } else if self.rising || aim > self.budget * (1.0 + hysteresis) {
            let growth = if config.doubling.is_zero() {
                f64::INFINITY
            } else {
                (deadline / config.doubling.as_secs_f64()).exp2()
            };
            self.budget = (self.budget * growth).min(aim);
            self.rising = self.budget < aim;
        }
    }

    fn clamp(&self, units: f64) -> f64 {
        units.min(self.max).max(self.floor.min(self.max))
    }
}

/// The share of the way a value moves in one block of `block` seconds
/// towards a new one, for a time constant `time`.
fn coefficient(block: f64, time: Duration) -> f64 {
    let time = time.as_secs_f64();
    if time <= 0.0 {
        1.0
    } else {
        -(-block / time).exp_m1()
    }
}

/// A block's budget being spent, to hand out in the instrument's order of
/// priority: the most audible voices first, at their best tier, and
/// cheaper tiers or nothing for the rest.
///
/// ```
/// use manguebyte_core::governor::Allowance;
///
/// let mut allowance = Allowance::new(10.0);
/// assert!(allowance.try_take(6.0));
/// assert!(!allowance.try_take(6.0)); // the full tier no longer fits
/// assert!(allowance.try_take(3.0)); // a cheaper one does
/// assert_eq!(allowance.take_up_to(4.0), 1.0); // one partial of four
/// assert_eq!(allowance.spent(), 10.0);
/// assert!(allowance.limited());
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Allowance {
    budget: f64,
    spent: f64,
    limited: bool,
}

impl Allowance {
    /// An allowance of `budget` units, none spent.
    pub fn new(budget: f64) -> Allowance {
        Allowance {
            budget: if budget.is_nan() { 0.0 } else { budget },
            spent: 0.0,
            limited: false,
        }
    }

    /// Spends `cost` if it fits in what is left, and says whether it did.
    /// A cost that does not fit marks the allowance [limited](Self::limited).
    pub fn try_take(&mut self, cost: f64) -> bool {
        let fits = cost <= self.remaining();
        if fits {
            self.spent += cost.max(0.0);
        } else {
            self.limited = true;
        }
        fits
    }

    /// Spends as much of `cost` as is left, in whole units, and returns
    /// how much it spent. Spending less than `cost` marks the allowance
    /// [limited](Self::limited).
    pub fn take_up_to(&mut self, cost: f64) -> f64 {
        let taken = cost.min(self.remaining().floor()).max(0.0);
        self.spent += taken;
        self.limited |= taken < cost;
        taken
    }

    /// Marks the allowance limited: something the instrument wanted was
    /// left out for want of budget, in a way the allowance did not see.
    pub fn mark_limited(&mut self) {
        self.limited = true;
    }

    /// Whether something was refused or cut short: the instrument wanted
    /// more than its budget. What to give [`Governor::end_block`].
    pub fn limited(&self) -> bool {
        self.limited
    }

    /// Spends `cost` whether or not it fits: work that must be done.
    pub fn force(&mut self, cost: f64) {
        self.spent += cost.max(0.0);
    }

    /// The units not yet spent (none once more was forced than the budget).
    pub fn remaining(&self) -> f64 {
        (self.budget - self.spent).max(0.0)
    }

    /// The units spent: what to give [`Governor::end_block`].
    pub fn spent(&self) -> f64 {
        self.spent
    }

    /// The budget it was made with.
    pub fn budget(&self) -> f64 {
        self.budget
    }
}
