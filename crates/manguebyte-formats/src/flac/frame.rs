//! Turning one block of samples into one frame (RFC 9639, section 9).
//!
//! Each channel becomes a subframe, coded in whichever of the format's ways
//! comes out smallest: as one value, as plain samples, or as the first few
//! samples and the residual of a fixed or a linear predictor. Two channels
//! may first be turned into their sum or difference, which for most music
//! is quieter than either channel and so takes fewer bits.

use super::bits::BitWriter;
use super::lpc::{self, MAX_ORDER, Predictors, Quantized, Window};
use super::rice::{self, RESIDUAL_LIMIT, ResidualPlan};
use super::{crc8, crc16};

/// How two channels are combined before coding (RFC 9639, section 4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stereo {
    Independent,
    /// The left channel and the difference, left minus right.
    LeftSide,
    /// The difference and the right channel.
    SideRight,
    /// The mean of the two, rounded down, and the difference.
    MidSide,
}

impl Stereo {
    /// The four, in the order they are tried. The slots of the encoder hold
    /// left, right, side and mid, and each pair names the two coded.
    const ALL: [(Self, [usize; 2]); 4] = [
        (Self::Independent, [0, 1]),
        (Self::LeftSide, [0, 2]),
        (Self::SideRight, [2, 1]),
        (Self::MidSide, [3, 2]),
    ];

    fn code(self, channels: usize) -> u8 {
        match self {
            Self::Independent => channels as u8 - 1,
            Self::LeftSide => 8,
            Self::SideRight => 9,
            Self::MidSide => 10,
        }
    }
}

/// How the stereo combination of a block is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StereoSearch {
    /// From a quick estimate of each of the four channels' size.
    Estimate,
    /// By coding all four channels in full and comparing.
    Exact,
}

const WHOLE: &[Window] = &[Window::WHOLE];
const HALVES: &[Window] = &[
    Window::WHOLE,
    Window { part: 0, parts: 2 },
    Window { part: 1, parts: 2 },
];
const THIRDS: &[Window] = &[
    Window::WHOLE,
    Window { part: 0, parts: 2 },
    Window { part: 1, parts: 2 },
    Window { part: 0, parts: 3 },
    Window { part: 1, parts: 3 },
    Window { part: 2, parts: 3 },
];

/// The most windows any level tries.
const MAX_WINDOWS: usize = 6;

/// How hard the encoder looks for a small coding: what a compression level
/// means.
#[derive(Debug, Clone, Copy)]
pub(super) struct Settings {
    /// Samples in a block unless the caller says otherwise.
    pub(super) block_size: u32,
    /// Zero for fixed predictors only.
    max_lpc_order: usize,
    windows: &'static [Window],
    /// How many pairs of window and order are coded in trial, out of all
    /// those whose size was estimated, best first.
    lpc_tries: usize,
    /// Coefficient precisions tried, as differences from the one the block
    /// size suggests.
    precisions: &'static [i32],
    max_partition_order: u32,
    stereo: StereoSearch,
}

impl Settings {
    /// The settings of a compression level from 0 to 8.
    ///
    /// The shape follows the presets of the reference `flac` tool as its
    /// manual describes them: fixed predictors in blocks of 1152 at the
    /// lowest levels, linear prediction in blocks of 4096 from level 3, and
    /// more orders and residual partitions the higher the level. The search
    /// itself is this encoder's own.
    pub(super) fn level(level: u8) -> Self {
        let fixed = |max_partition_order, stereo| Self {
            block_size: 1152,
            max_lpc_order: 0,
            windows: WHOLE,
            lpc_tries: 0,
            precisions: &[0],
            max_partition_order,
            stereo,
        };
        let lpc =
            |max_lpc_order, windows, lpc_tries, precisions, max_partition_order, stereo| Self {
                block_size: 4096,
                max_lpc_order,
                windows,
                lpc_tries,
                precisions,
                max_partition_order,
                stereo,
            };
        use StereoSearch::{Estimate, Exact};
        match level {
            0 => fixed(3, Estimate),
            1 => fixed(4, Estimate),
            2 => fixed(4, Exact),
            3 => lpc(6, WHOLE, 1, &[0], 4, Estimate),
            4 => lpc(8, WHOLE, 1, &[0], 5, Estimate),
            5 => lpc(8, WHOLE, 2, &[0], 5, Exact),
            6 => lpc(8, HALVES, 3, &[-1, 0, 1], 6, Exact),
            7 => lpc(12, HALVES, 6, &[-1, 0, 1], 6, Exact),
            _ => lpc(12, THIRDS, 12, &[-2, -1, 0, 1, 2], 8, Exact),
        }
    }

    #[cfg(test)]
    pub(super) fn exact_stereo(&self) -> bool {
        self.stereo == StereoSearch::Exact
    }

    #[cfg(test)]
    pub(super) fn max_lpc_order(&self) -> usize {
        self.max_lpc_order
    }

    #[cfg(test)]
    pub(super) fn max_partition_order(&self) -> u32 {
        self.max_partition_order
    }
}

/// How a subframe is coded.
#[derive(Debug, Clone, Copy)]
enum Model {
    Constant(i64),
    Verbatim,
    Fixed(usize),
    Lpc(Quantized),
}

/// The plan for one subframe.
#[derive(Debug, Clone)]
struct Subframe {
    model: Model,
    /// Bits of each stored sample: the channel's depth, one more for a side
    /// channel, less the wasted bits.
    bits: u32,
    /// Low bits that are zero in every sample and are not stored.
    wasted: u32,
    residual: ResidualPlan,
    /// Bits the whole subframe takes.
    cost: u64,
}

impl Subframe {
    fn new() -> Self {
        Self {
            model: Model::Verbatim,
            bits: 0,
            wasted: 0,
            residual: ResidualPlan::new(),
            cost: 0,
        }
    }
}

/// One way of predicting a block, ranked by an estimate of its size.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    estimate: f64,
    window: usize,
    order: usize,
}

/// Encodes blocks into frames. Everything it needs is allocated when it is
/// made, for the largest block it will be given.
pub(super) struct FrameEncoder {
    settings: Settings,
    channels: usize,
    bits: u32,
    /// The sample rate field of the frame header, and the bits that follow
    /// the header when the rate has no code of its own.
    rate_code: u8,
    rate_extra: Option<(u64, u32)>,
    /// The sample size field of the frame header.
    size_code: u8,
    /// Left, right, side and mid for two channels; the first holds each
    /// channel in turn otherwise. Wasted bits are shifted out in place.
    signals: [Vec<i64>; 4],
    residuals: [Vec<i64>; 4],
    plans: [Subframe; 4],
    /// Residuals of the predictor being tried.
    trial: Vec<i64>,
    windowed: Vec<f64>,
    windows: Vec<Vec<f64>>,
    /// The sum of the squares of each window, which turns an error energy
    /// into an error per sample.
    window_energy: [f64; MAX_WINDOWS],
    /// The block length the windows were made for.
    window_len: usize,
    predictors: Vec<Predictors>,
    candidates: [Candidate; MAX_WINDOWS * MAX_ORDER],
}

impl FrameEncoder {
    /// An encoder for blocks of up to `block_size` samples in each of
    /// `channels` channels of `bits` bits.
    pub(super) fn new(
        settings: Settings,
        channels: usize,
        bits: u32,
        sample_rate: u32,
        block_size: usize,
    ) -> Self {
        let (rate_code, rate_extra) = rate_code(sample_rate);
        let slots = if channels == 2 { 4 } else { 1 };
        let buffer = |slot: usize| {
            if slot < slots {
                vec![0i64; block_size]
            } else {
                Vec::new()
            }
        };
        let lpc = settings.max_lpc_order > 0;
        let window_count = if lpc { settings.windows.len() } else { 0 };
        Self {
            settings,
            channels,
            bits,
            rate_code,
            rate_extra,
            size_code: size_code(bits),
            signals: std::array::from_fn(buffer),
            residuals: std::array::from_fn(buffer),
            plans: std::array::from_fn(|_| Subframe::new()),
            trial: vec![0; block_size],
            windowed: if lpc {
                vec![0.0; block_size]
            } else {
                Vec::new()
            },
            windows: (0..window_count).map(|_| vec![0.0; block_size]).collect(),
            window_energy: [0.0; MAX_WINDOWS],
            window_len: 0,
            predictors: (0..window_count).map(|_| Predictors::new()).collect(),
            candidates: [Candidate {
                estimate: 0.0,
                window: 0,
                order: 0,
            }; MAX_WINDOWS * MAX_ORDER],
        }
    }

    /// The most bytes a frame of `block_size` samples can take: a header,
    /// every channel stored as plain samples one bit wider than the stream,
    /// and the checksum. No subframe is ever larger than its samples stored
    /// plainly, so no frame exceeds this.
    pub(super) fn max_frame_bytes(channels: usize, bits: u32, block_size: usize) -> usize {
        16 + channels * (6 + (block_size * (bits as usize + 1)).div_ceil(8)) + 2
    }

    /// Encodes the first `len` samples of each channel of `block` as frame
    /// number `number`, into `out`.
    pub(super) fn encode(
        &mut self,
        block: &[Vec<i32>],
        len: usize,
        number: u64,
        out: &mut BitWriter,
    ) {
        out.clear();
        if self.channels == 2 {
            let stereo = self.plan_stereo(block, len);
            let slots = Stereo::ALL
                .iter()
                .find(|(kind, _)| *kind == stereo)
                .map_or([0, 1], |(_, slots)| *slots);
            self.write_header(out, len, number, stereo.code(2));
            for slot in slots {
                self.write_subframe(slot, len, out);
            }
        } else {
            self.write_header(out, len, number, Stereo::Independent.code(self.channels));
            for samples in block.iter().take(self.channels) {
                load(&mut self.signals[0], &samples[..len]);
                self.plan(0, len, self.bits);
                self.write_subframe(0, len, out);
            }
        }
        out.align();
        let checksum = crc16(out.bytes());
        out.write(u64::from(checksum), 16);
    }

    /// Chooses how to combine two channels, and plans the subframes of the
    /// two that will be coded.
    fn plan_stereo(&mut self, block: &[Vec<i32>], len: usize) -> Stereo {
        let (left, right) = (&block[0][..len], &block[1][..len]);
        let [l, r, s, m] = &mut self.signals;
        for (index, (&a, &b)) in left.iter().zip(right).enumerate() {
            let (a, b) = (i64::from(a), i64::from(b));
            l[index] = a;
            r[index] = b;
            s[index] = a - b;
            m[index] = (a + b) >> 1;
        }
        let bits = self.bits;
        let depth = move |slot: usize| bits + u32::from(slot == 2);
        let pair_cost = |costs: &[u64; 4], slots: [usize; 2]| costs[slots[0]] + costs[slots[1]];

        match self.settings.stereo {
            StereoSearch::Exact => {
                let mut costs = [0u64; 4];
                for (slot, cost) in costs.iter_mut().enumerate() {
                    self.plan(slot, len, depth(slot));
                    *cost = self.plans[slot].cost;
                }
                Stereo::ALL
                    .iter()
                    .min_by_key(|(_, slots)| pair_cost(&costs, *slots))
                    .map_or(Stereo::Independent, |(kind, _)| *kind)
            }
            StereoSearch::Estimate => {
                let mut estimates = [0u64; 4];
                for (slot, estimate) in estimates.iter_mut().enumerate() {
                    *estimate = self.quick_estimate(slot, len, depth(slot));
                }
                let (stereo, slots) = Stereo::ALL
                    .iter()
                    .min_by_key(|(_, slots)| pair_cost(&estimates, *slots))
                    .copied()
                    .unwrap_or(Stereo::ALL[0]);
                for slot in slots {
                    self.plan(slot, len, depth(slot));
                }
                // An estimate can be wrong. The independent channels stored
                // plainly are the most a frame should ever cost, so if the
                // choice came out above that, the channels are coded
                // independently after all. Planning shifted the chosen
                // slots in place, so the two channels are loaded again.
                let plain = 2 * (8 + len as u64 * u64::from(self.bits));
                let cost = self.plans[slots[0]].cost + self.plans[slots[1]].cost;
                if stereo != Stereo::Independent && cost > plain {
                    for (slot, samples) in [left, right].into_iter().enumerate() {
                        for (to, &from) in self.signals[slot].iter_mut().zip(samples) {
                            *to = i64::from(from);
                        }
                        self.plan(slot, len, self.bits);
                    }
                    return Stereo::Independent;
                }
                stereo
            }
        }
    }

    /// A rough size for the channel in `slot`: the best fixed predictor,
    /// with its residual coded as one partition, or the plain samples if
    /// they are smaller.
    fn quick_estimate(&mut self, slot: usize, len: usize, depth: u32) -> u64 {
        let signal = &self.signals[slot][..len];
        let mut best = len as u64 * u64::from(depth);
        for order in 0..=4.min(len - 1) {
            if lpc::fixed_residual(signal, order, RESIDUAL_LIMIT, &mut self.trial) {
                let residual = &self.trial[..len - order];
                let cost =
                    order as u64 * u64::from(depth) + rice::estimate(residual, order, len, 0);
                best = best.min(cost);
            }
        }
        best
    }

    /// Makes the windows for blocks of `len` samples, unless they were made
    /// for that length already.
    fn prepare_windows(&mut self, len: usize) {
        if self.window_len == len {
            return;
        }
        for ((window, values), energy) in self
            .settings
            .windows
            .iter()
            .zip(self.windows.iter_mut())
            .zip(self.window_energy.iter_mut())
        {
            window.fill(&mut values[..len]);
            *energy = values[..len].iter().map(|weight| weight * weight).sum();
        }
        self.window_len = len;
    }

    /// Finds the smallest coding of the samples in `slot`, stored in `depth`
    /// bits, and puts it in `self.plans[slot]`, with the residual it needs in
    /// `self.residuals[slot]`.
    fn plan(&mut self, slot: usize, len: usize, depth: u32) {
        let signal = &mut self.signals[slot][..len];
        let plan = &mut self.plans[slot];
        let first = signal[0];
        if signal.iter().all(|&sample| sample == first) {
            plan.model = Model::Constant(first);
            plan.bits = depth;
            plan.wasted = 0;
            plan.cost = 8 + u64::from(depth);
            return;
        }

        // Low bits that are zero in every sample need not be stored. Not
        // every sample is zero here, so fewer than `depth` bits are wasted.
        let wasted = signal
            .iter()
            .fold(0i64, |bits, &sample| bits | sample)
            .trailing_zeros()
            .min(depth - 1);
        if wasted > 0 {
            for sample in signal.iter_mut() {
                *sample >>= wasted;
            }
        }
        let bits = depth - wasted;
        let n = len as u64;
        let width = u64::from(bits);
        // The zero bit, the type, the wasted bits flag and its count.
        let header = 8 + u64::from(wasted);
        let plain = header + n * width;
        let limit = self.settings.max_partition_order;

        let mut best = (plain, Model::Verbatim);
        for order in 0..=4.min(len - 1) {
            if lpc::fixed_residual(signal, order, RESIDUAL_LIMIT, &mut self.trial) {
                let residual = &self.trial[..len - order];
                let cost =
                    header + order as u64 * width + rice::estimate(residual, order, len, limit);
                if cost < best.0 {
                    best = (cost, Model::Fixed(order));
                }
            }
        }

        let max_order = self.settings.max_lpc_order.min(len - 1);
        if max_order > 0 {
            if let Some(found) = self.search_lpc(slot, len, bits, header, max_order) {
                if found.0 < best.0 {
                    best = found;
                }
            }
        }

        // Code the winner exactly. Its residual goes where it will be
        // written from.
        let signal = &self.signals[slot][..len];
        let plan = &mut self.plans[slot];
        let residual = &mut self.residuals[slot];
        plan.bits = bits;
        plan.wasted = wasted;
        plan.model = Model::Verbatim;
        plan.cost = plain;
        let exact = match best.1 {
            Model::Fixed(order) => {
                lpc::fixed_residual(signal, order, RESIDUAL_LIMIT, residual);
                let residual = &residual[..len - order];
                let coded = rice::plan(residual, order, len, limit, &mut plan.residual);
                Some(header + order as u64 * width + coded)
            }
            Model::Lpc(predictor) => {
                let order = predictor.order;
                lpc::residual(signal, &predictor, RESIDUAL_LIMIT, residual);
                let residual = &residual[..len - order];
                let coded = rice::plan(residual, order, len, limit, &mut plan.residual);
                let fields = 4 + 5 + order as u64 * u64::from(predictor.precision);
                Some(header + order as u64 * width + fields + coded)
            }
            _ => None,
        };
        // The estimate that chose the model is never below the exact size,
        // so this holds; it is checked so that no estimate can make a
        // subframe larger than its plain samples.
        if let Some(cost) = exact.filter(|&cost| cost < plain) {
            plan.model = best.1;
            plan.cost = cost;
        }
    }

    /// Looks for the best linear predictor for the samples in `slot`, and
    /// returns its estimated cost with it.
    fn search_lpc(
        &mut self,
        slot: usize,
        len: usize,
        bits: u32,
        header: u64,
        max_order: usize,
    ) -> Option<(u64, Model)> {
        self.prepare_windows(len);
        let signal = &self.signals[slot][..len];
        let limit = self.settings.max_partition_order;
        let suggested = suggested_precision(len);
        let width = u64::from(bits);

        // Every order of every window, ranked by what its prediction error
        // says it would cost.
        let mut count = 0;
        for (index, window) in self.settings.windows.iter().enumerate() {
            let mut correlation = [0.0; MAX_ORDER + 1];
            lpc::autocorrelation(
                signal,
                &self.windows[index][..len],
                window.support(len),
                &mut self.windowed,
                max_order,
                &mut correlation,
            );
            let predictors = &mut self.predictors[index];
            predictors.solve(&correlation, max_order);
            let energy = self.window_energy[index];
            // A sum of squares of finite weights: zero, or more.
            if energy <= 0.0 {
                continue;
            }
            for order in 1..=predictors.max_order {
                // A residual of variance v takes about log2(v) / 2 bits a
                // sample, plus a constant that is the same for all.
                let variance = predictors.error[order] / energy;
                let per_sample = (0.5 * variance.max(1e-12).log2() + 1.5).max(0.0);
                let overhead = order as f64 * f64::from(bits + suggested);
                self.candidates[count] = Candidate {
                    estimate: (len - order) as f64 * per_sample + overhead,
                    window: index,
                    order,
                };
                count += 1;
            }
        }
        let candidates = &mut self.candidates[..count];
        candidates.sort_unstable_by(|a, b| a.estimate.total_cmp(&b.estimate));
        let tries = count.min(self.settings.lpc_tries);

        // The cost of one candidate at one precision, coded in trial.
        let mut cost_of = |candidate: Candidate, precision: u32| {
            let order = candidate.order;
            let coefficients = &self.predictors[candidate.window].coefficients[order - 1][..order];
            let predictor = lpc::quantize(coefficients, precision)?;
            if !lpc::residual(signal, &predictor, RESIDUAL_LIMIT, &mut self.trial) {
                return None;
            }
            let residual = &self.trial[..len - order];
            let fields = 4 + 5 + order as u64 * u64::from(predictor.precision);
            let cost = header
                + order as u64 * width
                + fields
                + rice::estimate(residual, order, len, limit);
            Some((cost, Model::Lpc(predictor)))
        };

        // The candidates the estimate ranks best are coded at the suggested
        // precision, and the two that come out smallest at the others too.
        let mut best: Option<(u64, Model)> = None;
        let mut coded = [(u64::MAX, 0usize); MAX_WINDOWS * MAX_ORDER];
        for (index, candidate) in self.candidates[..tries].iter().enumerate() {
            if let Some(found) = cost_of(*candidate, suggested) {
                coded[index] = (found.0, index);
                if best.is_none_or(|(least, _)| found.0 < least) {
                    best = Some(found);
                }
            }
        }
        let coded = &mut coded[..tries];
        coded.sort_unstable();
        for &(cost, index) in coded.iter().take(2) {
            if cost == u64::MAX {
                break;
            }
            for &offset in self
                .settings
                .precisions
                .iter()
                .filter(|&&offset| offset != 0)
            {
                let precision = (suggested as i32 + offset).clamp(5, 15) as u32;
                if let Some(found) = cost_of(self.candidates[index], precision) {
                    if best.is_none_or(|(least, _)| found.0 < least) {
                        best = Some(found);
                    }
                }
            }
        }
        best
    }

    /// Writes the frame header (RFC 9639, section 9.1).
    fn write_header(&self, out: &mut BitWriter, len: usize, number: u64, channel_code: u8) {
        // The sync code, a reserved zero and "fixed block size".
        out.write(0b1111_1111_1111_1000, 16);
        let (block_code, block_extra) = block_code(len);
        out.write(u64::from(block_code), 4);
        out.write(u64::from(self.rate_code), 4);
        out.write(u64::from(channel_code), 4);
        out.write(u64::from(self.size_code), 3);
        out.write(0, 1);
        write_coded_number(out, number);
        if let Some((value, bits)) = block_extra {
            out.write(value, bits);
        }
        if let Some((value, bits)) = self.rate_extra {
            out.write(value, bits);
        }
        debug_assert!(out.is_aligned());
        let checksum = crc8(out.bytes());
        out.write(u64::from(checksum), 8);
    }

    /// Writes the subframe planned for `slot` (RFC 9639, section 9.2).
    fn write_subframe(&self, slot: usize, len: usize, out: &mut BitWriter) {
        let plan = &self.plans[slot];
        let signal = &self.signals[slot][..len];
        let kind = match plan.model {
            Model::Constant(_) => 0,
            Model::Verbatim => 1,
            Model::Fixed(order) => 8 + order as u64,
            Model::Lpc(predictor) => 31 + predictor.order as u64,
        };
        out.write(kind, 7);
        if plan.wasted > 0 {
            out.write(1, 1);
            out.write_unary(u64::from(plan.wasted - 1));
        } else {
            out.write(0, 1);
        }
        match plan.model {
            Model::Constant(value) => out.write_signed(value, plan.bits),
            Model::Verbatim => {
                for &sample in signal {
                    out.write_signed(sample, plan.bits);
                }
            }
            Model::Fixed(order) => {
                for &sample in &signal[..order] {
                    out.write_signed(sample, plan.bits);
                }
                let residual = &self.residuals[slot][..len - order];
                rice::write(out, residual, order, len, &plan.residual);
            }
            Model::Lpc(predictor) => {
                let order = predictor.order;
                for &sample in &signal[..order] {
                    out.write_signed(sample, plan.bits);
                }
                out.write(u64::from(predictor.precision - 1), 4);
                out.write(u64::from(predictor.shift), 5);
                for &coefficient in &predictor.coefficients[..order] {
                    out.write_signed(i64::from(coefficient), predictor.precision);
                }
                let residual = &self.residuals[slot][..len - order];
                rice::write(out, residual, order, len, &plan.residual);
            }
        }
    }
}

/// Copies a channel into a slot.
fn load(slot: &mut [i64], samples: &[i32]) {
    for (to, &from) in slot.iter_mut().zip(samples) {
        *to = i64::from(from);
    }
}

/// The coefficient precision tried first for blocks of `len` samples.
///
/// Every coefficient costs its precision in bits once per block, while a
/// better prediction saves bits on every sample, so long blocks can afford
/// more precision than short ones.
fn suggested_precision(len: usize) -> u32 {
    (usize::BITS - 1 - len.max(1).leading_zeros()).clamp(7, 13)
}

/// The block size field of a frame header for `len` samples, and the bits
/// that follow the header when the size has no code of its own (RFC 9639,
/// section 9.1.1).
fn block_code(len: usize) -> (u8, Option<(u64, u32)>) {
    match len {
        192 => (1, None),
        576 => (2, None),
        1152 => (3, None),
        2304 => (4, None),
        4608 => (5, None),
        256 => (8, None),
        512 => (9, None),
        1024 => (10, None),
        2048 => (11, None),
        4096 => (12, None),
        8192 => (13, None),
        16384 => (14, None),
        32768 => (15, None),
        _ if len <= 256 => (6, Some((len as u64 - 1, 8))),
        _ => (7, Some((len as u64 - 1, 16))),
    }
}

/// The sample rate field of a frame header (RFC 9639, section 9.1.2).
///
/// A rate without a code of its own is stated after the header if it fits
/// one of the three ways the format allows, and otherwise left to the
/// stream information, which puts the stream outside the streamable subset.
fn rate_code(rate: u32) -> (u8, Option<(u64, u32)>) {
    let rate64 = u64::from(rate);
    match rate {
        88_200 => (1, None),
        176_400 => (2, None),
        192_000 => (3, None),
        8_000 => (4, None),
        16_000 => (5, None),
        22_050 => (6, None),
        24_000 => (7, None),
        32_000 => (8, None),
        44_100 => (9, None),
        48_000 => (10, None),
        96_000 => (11, None),
        _ if rate % 1000 == 0 && rate / 1000 <= 255 => (12, Some((rate64 / 1000, 8))),
        _ if rate <= 65_535 => (13, Some((rate64, 16))),
        _ if rate % 10 == 0 && rate / 10 <= 65_535 => (14, Some((rate64 / 10, 16))),
        _ => (0, None),
    }
}

/// The sample size field of a frame header (RFC 9639, section 9.1.4). Depths
/// without a code are left to the stream information.
fn size_code(bits: u32) -> u8 {
    match bits {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        32 => 7,
        _ => 0,
    }
}

/// Writes a frame number in the coding of RFC 9639, section 9.1.5: UTF-8,
/// carried on to 36 bits.
fn write_coded_number(out: &mut BitWriter, number: u64) {
    if number < 0x80 {
        out.write(number, 8);
        return;
    }
    // A run of `bytes` ones, a zero and the top bits in the first byte, six
    // bits in each of the others.
    let mut bytes = 2u32;
    while bytes < 7 && number >> (6 * (bytes - 1) + 7 - bytes) != 0 {
        bytes += 1;
    }
    let lead = (0xFF00u64 >> bytes) & 0xFF;
    out.write(lead | (number >> (6 * (bytes - 1))), 8);
    for index in (0..bytes - 1).rev() {
        out.write(0x80 | ((number >> (6 * index)) & 0x3F), 8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_numbers_are_coded_like_utf8() {
        // The same values the decoder's test reads, and the bytes UTF-8
        // gives for the code points that share them.
        for (number, bytes) in [
            (0u64, &[0x00][..]),
            (0x7F, &[0x7F]),
            (0x80, &[0xC2, 0x80]),
            (0x7FF, &[0xDF, 0xBF]),
            (0x800, &[0xE0, 0xA0, 0x80]),
            (0xFFFF, &[0xEF, 0xBF, 0xBF]),
            (0x1_0000, &[0xF0, 0x90, 0x80, 0x80]),
            (0x1F_FFFF, &[0xF7, 0xBF, 0xBF, 0xBF]),
            (0x3FF_FFFF, &[0xFB, 0xBF, 0xBF, 0xBF, 0xBF]),
            (0x7FFF_FFFF, &[0xFD, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF]),
            (0xF_FFFF_FFFF, &[0xFE, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF]),
        ] {
            let mut out = BitWriter::with_capacity(8);
            write_coded_number(&mut out, number);
            assert_eq!(out.bytes(), bytes, "{number:#X}");
        }
        for code_point in ['\u{0}', 'é', '€', '\u{10FFFF}'] {
            let mut out = BitWriter::with_capacity(8);
            write_coded_number(&mut out, u64::from(u32::from(code_point)));
            let mut utf8 = [0u8; 4];
            assert_eq!(out.bytes(), code_point.encode_utf8(&mut utf8).as_bytes());
        }
    }

    #[test]
    fn header_codes_cover_the_common_values() {
        assert_eq!(block_code(4096), (12, None));
        assert_eq!(block_code(1152), (3, None));
        assert_eq!(block_code(1), (6, Some((0, 8))));
        assert_eq!(block_code(256), (8, None));
        assert_eq!(block_code(257), (7, Some((256, 16))));
        assert_eq!(block_code(65_535), (7, Some((65_534, 16))));
        assert_eq!(rate_code(44_100), (9, None));
        assert_eq!(rate_code(11_000), (12, Some((11, 8))));
        assert_eq!(rate_code(11_025), (13, Some((11_025, 16))));
        assert_eq!(rate_code(352_800), (14, Some((35_280, 16))));
        assert_eq!(rate_code(1_000_001), (0, None));
        assert_eq!(size_code(24), 6);
        assert_eq!(size_code(18), 0);
        assert_eq!(suggested_precision(4096), 12);
        assert_eq!(suggested_precision(192), 7);
        assert_eq!(suggested_precision(65_535), 13);
    }
}
