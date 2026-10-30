# ADR 0018: A governor that gives an instrument a budget to spend

Status: accepted, 2026-10-30

## Context

An instrument can be asked for more than the machine can render. Caecilia
2.0, a pipe organ built on Manguebyte, with every stop and coupler drawn on
a weak laptop, wants several times what fits in a block's time on any
number of cores (ADR 0015). Missing the deadline is a glitch. An organ can
degrade instead: render a pipe with fewer partials or from a cheaper model,
drop the quietest partials, steal the least audible pipes. What it cannot
know by itself is how far to go: that depends on the machine, on what else
runs on it, on the processor's clock at that moment.

JUCE has `AudioProcessLoadMeasurer`, which measures: a processor wraps its
callback in a scoped timer and reads back a smoothed load and a count of
overruns. It gives no budget and takes no part in what the processor does
next, and nothing in JUCE's modules does.

The rules of ADR 0002 hold: whatever runs on the audio thread must not
allocate, lock or wait, and must not call the system unboundedly. Offline
renders must be the same on every machine.

## Decision

**1. `manguebyte_core::governor`, in the core crate.** `Governor<C: Clock>`,
`GovernorConfig`, `GovernorStats`, `Allowance`, `Clock` and `SystemClock`.
It needs only `std::time`, so the crate keeps no dependencies, and it sits
beside the render pool (ADR 0015), whose missed jobs it reads.

**2. Abstract cost units, relative costs declared, one scale learnt.** The
instrument decides what a unit is (a partial, a sixteenth of a voice) and
what its tiers cost in units; it spends the budget in its own order of
priority (`Allowance` helps) and reports what it spent and whether it was
*limited*, wanting more than it got. The governor learns one number, the
load of one unit as a fraction of the deadline, and gives as budget the
units that the estimate says fit under the target. Units are per block of
any length, since a voice's time and the deadline both grow with the
frames.

The alternatives were worse. Declaring costs in seconds ties the instrument
to the machine it was measured on, and the cost of a unit changes on one
machine with its clock and what else runs. Learning a cost per kind of
thing (per voice of each tier, per partial) is a regression on a few
hundred noisy samples whose inputs move together, since the instrument
spends in a fixed order: badly conditioned, and not needed, because the
relative costs follow from the kernels and the instrument knows them.

**3. Fixed work is not modelled, and does not need to be.** With fixed work
`o` and `c` per unit, a block of `u` units loads `o + c u`, and the
estimate `x = (o + c u) / u` is above `c`, which errs on the safe side.
When the instrument is held by its budget, the budget is
`B = a / x` for the aim `a`; the fixed point of `B = a B / (o + c B)` is
`o + c B = a`, the load aimed at, whatever `o` is. Linearised there, one
step multiplies the error by `1 - k (1 - o / a)` for a step size `k` in
(0, 1], so it converges without overshoot as long as the fixed work is
less than the aim. Where the instrument is not held, a block whose load is
under half the target (`learn_above`, 35 % of the deadline) is not learnt
from, so fixed work inflates the estimate by at most a factor of
`1 + o / (0.35 - o)`: 17 % for fixed work of 5 % of the deadline.

**4. How it moves.** The estimate moves towards a block's higher load per
unit by half the way (`attack`), all the way when that load is above the
ceiling (90 %), never by more than a factor of two in one block
(`max_rise`), and towards a lower one with a time constant of half a
second (`release`); with `spike_blocks(n)` it goes by the lowest load per
unit of the last `n` blocks learnt from (one unless set). The budget aims at the target (70 %) less the
hysteresis (5 %): it falls at once when the aim drops below it by the
hysteresis or the load passes the target, starts rising only when the aim
is the hysteresis above it, and then rises by at most a factor of two
every half second (`doubling`) until it reaches the aim. A quiet or silent
block leaves everything as it was, so the first chord after a rest gets the
last chord's budget; a limited block in which nothing fitted lets the
estimate fall at the release rate, so the budget cannot stay below the
cheapest thing the instrument renders.

**5. Spikes: what the real clock taught.** The first version believed every
block, as asked ("a fast reaction to spikes"), without bound. On the real
clock, on the development laptop, one block during which the thread was
preempted for 56 ms (2118 % of the deadline) multiplied the estimate
twentyfold, the budget fell below the organ's cheapest pipe, nothing was
rendered, and with nothing spent the governor learnt nothing more: the
organ stayed silent. Two changes fix that for good. The instrument says
whether it was limited, and a limited block in which nothing fitted lets
the estimate fall at the release rate. One block raises the estimate by
at most `max_rise`.

What is left is a choice. Time stolen by the system does not shrink when
the instrument renders less, but neither can the governor tell it from a
machine that has become slower. Believing every block (the default)
answers within a block, and follows the costliest blocks; going by the
lowest of the last few (`spike_blocks(3)`) ignores a spike shorter than
that, and lets more of the blocks whose cost really varies be late.
`examples/governor_organ.rs`, a `PartialBank` organ asked for twice what
fits, measured on the development laptop (an i3-1215U, two performance
and four efficiency cores) while other programs used most of it (without
the governor, the organ's median block took 1.7 to 14 times its
calibrated time), at normal thread priority, 10 or 20 seconds a run:

| Setting | Blocks late | Organ rendered | Median load |
|---|---|---|---|
| default (`spike_blocks` 1, `max_rise` 2) | 0.3 to 0.7 % | 0.3 to 10 % | 1 to 32 % |
| `max_rise` 1.1 | 3.1 % | 8.7 % | 16 % |
| `spike_blocks` 3 | 6 to 39 % | 7 to 18 % | 28 to 46 % |
| `spike_blocks` 3, `max_rise` 1.25 | 5.3 % | 15 % | 50 % |
| no governor | 100 % | 100 % | 170 to 1430 % |

At normal priority, the default keeps the deadline, which is what the
governor is for, and pays for it in sound on a machine this busy: the
estimate climbs to the costliest blocks faster than the release brings
it down. Rendering 35 % would load the target at the calibrated cost.

On a real-time thread, which is where audio runs, the picture changes.
The example asks for MMCSS "Pro Audio" through the pool's `Promotion`
(`sleep-rt`), on the same busy machine, 20 seconds a run:

| Setting | Blocks late | Organ rendered | Load: median, 99th percentile, worst |
|---|---|---|---|
| default, two runs | none | 13 and 17 % | 55 to 57 %, 70 to 71 %, 89 to 98 % |
| `spike_blocks` 3 | none | 19 % | 62 %, 73 %, 96 % |
| default, spinning between blocks (`spin-rt`) | 0.06 % | 2.1 % | 8 %, 87 %, 110 % |
| default, normal priority, just before | 0.35 % | 9.9 % | 32 %, 69 %, 1325 % |

Asked for twice what fits, the governor held the load's 99th percentile at
the target and let no block be late. A thread that spins at real-time
priority between blocks, which no callback does, fared worse; why was not
looked into.

The fake-clock tests measure the same trade under a cost that moves by a
random amount every block: by default ±20 % moves nothing once settled,
the budget sitting where the costliest blocks fit, and ±50 % keeps it
within 3.6 % under and 9.7 % over its mean, turning round three times a
second; with `spike_blocks(3)`, ±50 % keeps it within 17 % under and 19 %
over, turning round nine times a second, nearer the target on average. In
none of these was a block late. A pool's missed jobs count as a whole
deadline's load and go through the same rules.

**6. The clock.** `Clock::now` gives a monotonic `Duration`; `SystemClock`
reads `Instant::now`, twice a block: `QueryPerformanceCounter` on Windows,
`clock_gettime` through the vDSO on Linux and Android, `clock_gettime`
with `CLOCK_UPTIME_RAW` on macOS and iOS, none of which enters the kernel
on current hardware (some virtual machines on Linux do). Tests give a fake
clock they move by hand, so every figure they check is the same on every
machine. `record` takes a duration the caller measured, for a block timed
elsewhere.

**7. Offline, floor, maximum.** `set_offline(true)` gives the maximum budget
every block, reads no clock and learns nothing, so an offline render is at
full quality and the same everywhere. `set_quality_floor` is the least the
budget gives, whatever the load: the plugin's choice, at the cost of the
deadline. `set_max_budget` is what the instrument costs at full quality,
if it knows; unlimited otherwise.

## Consequences

- An instrument that wants to degrade gracefully needs a cost model in
  units and an order of priority; the governor does not choose what to
  drop. `Allowance` makes the common case (best tier while it fits, then
  cheaper ones, then nothing) a few lines.
- Degrading is abrupt: the budget falls in one block, and a voice moved
  to a cheaper tier changes sound at once unless the instrument
  cross-fades its tiers. The budget rises slowly, so promotions are spread
  out, but each is still a step.
- On a busy machine at normal priority the default degrades far more
  than the work alone would need, since it cannot tell stolen time from
  cost; an instrument that would rather risk late blocks than thin out
  sets `spike_blocks` or a smaller `max_rise`.
- The first block after `new` or `reset`, with nothing learnt, renders the
  full demand; an instrument that knows it will overload can set a
  maximum.
- The governor is meant to run on a real-time thread, where preemption is
  rarer; there it held the load at the target on a busy laptop. It has
  not been run inside a host, on another system or with Caecilia.

## Alternatives not taken

- **A PID controller on the load.** It needs tuning per instrument, and the
  plant is not linear in the budget once fixed work matters; the
  multiplicative form above has one fixed point and no tuning beyond time
  constants.
- **A smoothed load alone, with the budget moved in fixed steps**
  (additive increase, multiplicative decrease). It forgets the cost during
  a rest, so the first chord after one overloads; and its steps are
  audible whatever their size.
- **A median of the recent loads** instead of their minimum, for
  `spike_blocks`. It rejects one spike in three but not two; stolen time
  only ever adds, so the minimum is the better estimate of what the work
  itself cost.
- **Tracking a quantile of the cost** (a step up on every block over the
  estimate, a smaller one down on every other, so that a chosen share of
  blocks is over). It would hold the share of blocks over the target, but
  if stolen time alone puts more blocks than that over, it climbs without
  end just the same; not tried.
- **Deciding inside the render pool.** The pool knows its jobs, not what a
  voice at a cheaper tier costs; the governor reads its misses instead.
