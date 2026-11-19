//! The CELT encoder: one frame of one or two channels at a time, into the
//! bytes it is given (RFC 6716, sections 4.3 and 5.3).
//!
//! Only the bitstream is normative, and this encoder makes simple choices
//! where the RFC leaves them to it:
//!
//! - The pre-emphasis of section 4.3.7.2 run forward, then the MDCT of the
//!   decoder's window, one long block or, where the energy of the input
//!   jumps (a transient), as many short ones as the frame holds.
//! - Band energies coarse (6 dB steps, predicted from the frame before and
//!   the band below, with the Laplace model of section 4.3.2.1), intra
//!   whenever that takes fewer bits than inter, and fine (section
//!   4.3.2.2), with the bits the allocation leaves at the end.
//! - No pitch pre-filter (its flag says off), no boost of single bands
//!   (dynalloc), the middle allocation trim and the normal spread.
//! - Time-frequency resolution kept as the blocks give it: short blocks
//!   stay short in every band.
//! - Stereo as mid and side with the angle of each band, intensity stereo
//!   from a band the bitrate sets, and dual stereo, the channels apart,
//!   when they are sparser apart than together (section 4.3.4.4).
//! - Bands above the audio's own bandwidth, or that would get too few bits
//!   to be worth coding, skipped (section 4.3.3).
//!
//! What the frame codes goes through the very functions the decoder uses
//! to read it ([`compute_allocation`], [`quant_all_bands`]), with the
//! encoder's [`RangeEncoder`] in place of the decoder's: both work out the
//! same allocation and the same folded spectrum, and the encoder keeps the
//! state the decoder will have (`old_band_e`, the seed of the noise).

use super::bands::{BandScratch, FrameBands, haar1, quant_all_bands};
use super::mdct::Mdct;
use super::rate::{AllocChoices, MAX_FINE_BITS, caps, compute_allocation};
use super::tables::{
    BANDS, BETA_COEF, BETA_INTRA, E_MEANS, E_PROB_MODEL, EBANDS, MAX_LM, OVERLAP, PRED_COEF,
    PREEMPHASIS, SHORT_MDCT, SMALL_ENERGY_ICDF, SPREAD_ICDF, TF_SELECT, TRIM_ICDF,
};
use super::{MAX_FRAME, SPREAD_NORMAL};
use crate::opus::range::{BITRES, RangeEncoder};

/// The scale of the samples CELT works on: full scale is 32768.
const SCALE: f32 = 32_768.0;
/// Segments of 2.5 ms whose energy the transient detector compares.
const SEGMENT: usize = SHORT_MDCT;
/// How many segments before one its energy is compared with.
const HISTORY: usize = 8;
/// A segment this many times louder than those before is an attack.
const ATTACK: f32 = 10.0;
/// Segments of a block this far apart in energy call for short blocks.
const FALL: f32 = 1_000.0;
/// The lowest energy a band's log is taken of, as the decoder floors it.
const MIN_LOG_E: f32 = -28.0;

/// The size of a frame.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Budget {
    /// Exactly this many bytes.
    Fixed(usize),
    /// About `average` bytes for a frame of the usual kind, more for one of
    /// short blocks, fewer for one whose audio stops short of the top
    /// bands, two for silence, and never more than `max`.
    Variable { average: f32, max: usize },
}

/// What the stream asks of a frame besides its audio.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameChoices {
    /// The first band coded as intensity stereo; `BANDS` for none.
    pub(crate) intensity: usize,
    /// Whether dual stereo may be used where it pays.
    pub(crate) allow_dual: bool,
    /// Bands that would get fewer sixteenths of a bit per bin and channel
    /// than this are skipped.
    pub(crate) depth: i32,
}

/// Encodes the CELT frames of one stream of one or two channels.
pub(crate) struct CeltEncoder {
    channels: usize,
    /// The last `OVERLAP` samples of each channel, pre-emphasised, which
    /// the next block starts with.
    history: Vec<f32>,
    preemph_mem: [f32; 2],
    /// The quantised energies, as the decoder holds them.
    old_band_e: [f32; 2 * BANDS],
    /// The decoder's seed for its noise: the range of the frame before.
    rng: u32,
    /// The energy of the last segments, for the transient detector.
    segments: [f32; HISTORY],
    /// Whether a frame has been coded since the start, and whether the last
    /// one had an attack in its last segment, which the next block holds
    /// too.
    started: bool,
    attack_at_end: bool,
    mdct: Mdct,
    bands: BandScratch,
    /// The block of each channel: `OVERLAP` samples of history, then the
    /// frame.
    block: Vec<f32>,
    /// The spectrum of each channel, then the same normalised band by
    /// band.
    freq: Vec<f32>,
    x: Vec<f32>,
}

impl CeltEncoder {
    pub(crate) fn new(channels: usize) -> Self {
        debug_assert!((1..=2).contains(&channels));
        Self {
            channels,
            history: vec![0.0; channels * OVERLAP],
            preemph_mem: [0.0; 2],
            old_band_e: [0.0; 2 * BANDS],
            rng: 0,
            segments: [0.0; HISTORY],
            started: false,
            attack_at_end: false,
            mdct: Mdct::new(),
            bands: BandScratch::new(),
            block: vec![0.0; channels * (MAX_FRAME + OVERLAP)],
            freq: vec![0.0; 2 * MAX_FRAME],
            x: vec![0.0; 2 * MAX_FRAME],
        }
    }

    /// The range the decoder ends the last frame with, which both share
    /// when they agree on every symbol (`OPUS_GET_FINAL_RANGE`).
    #[cfg(test)]
    pub(crate) fn final_range(&self) -> u32 {
        self.rng
    }

    /// Encodes a frame of `frame_size` samples per channel (120, 240, 480
    /// or 960), held in `input` one channel after the other with full
    /// scale at 1, into a frame of the size `budget` gives, from 2 to 1275
    /// bytes.
    pub(crate) fn encode(
        &mut self,
        input: &[f32],
        frame_size: usize,
        budget: Budget,
        choices: &FrameChoices,
    ) -> Vec<u8> {
        let lm = match frame_size {
            120 => 0,
            240 => 1,
            480 => 2,
            _ => 3,
        };
        let m = 1usize << lm;
        let n = frame_size;
        let c = self.channels;
        let (start, end) = (0usize, BANDS);

        // The pre-emphasis, at the scale of the decoder.
        let stride = n + OVERLAP;
        let mut silent = true;
        let mut attack = false;
        for channel in 0..c {
            let block = &mut self.block[channel * stride..(channel + 1) * stride];
            block[..OVERLAP].copy_from_slice(&self.history[channel * OVERLAP..][..OVERLAP]);
            let mut memory = self.preemph_mem[channel];
            for (out, &sample) in block[OVERLAP..]
                .iter_mut()
                .zip(&input[channel * n..(channel + 1) * n])
            {
                let sample = SCALE * sample;
                *out = sample - memory;
                memory = PREEMPHASIS * sample;
            }
            self.preemph_mem[channel] = memory;
            silent &= block.iter().all(|&value| value == 0.0);
            self.history[channel * OVERLAP..][..OVERLAP].copy_from_slice(&block[n..]);
        }
        // Segment by segment, all channels together: an attack, against the
        // segments before, or a fall steep enough that the noise of a long
        // block would stand out at its end. The block starts with the last
        // segment of the frame before, so an attack there counts here too.
        let floor = SEGMENT as f32 * 3.0;
        let segments = n / SEGMENT;
        let mut loudest = self.segments[HISTORY - 1];
        let mut quietest = loudest;
        if self.attack_at_end {
            attack = true;
        }
        self.attack_at_end = false;
        for segment in 0..segments {
            let mut energy = floor;
            for channel in 0..c {
                let block = &self.block[channel * stride + OVERLAP..];
                energy += block[segment * SEGMENT..(segment + 1) * SEGMENT]
                    .iter()
                    .map(|&value| value * value)
                    .sum::<f32>();
            }
            let before = self.segments.iter().sum::<f32>() / HISTORY as f32;
            if self.started && energy > ATTACK * before {
                attack = true;
                self.attack_at_end = segment + 1 == segments;
            }
            loudest = loudest.max(energy);
            quietest = quietest.min(energy);
            self.segments.rotate_left(1);
            self.segments[HISTORY - 1] = energy;
        }
        if self.started && loudest > FALL * quietest {
            attack = true;
        }
        // A frame of two bytes or more has room for the flag, whatever the
        // flags before it take.
        let transient = attack && lm > 0;

        // The spectrum, and the energy and shape of each band.
        let short = transient;
        for channel in 0..c {
            let block = &self.block[channel * stride..(channel + 1) * stride];
            let freq = &mut self.freq[channel * n..(channel + 1) * n];
            if short {
                for b in 0..m {
                    self.mdct
                        .forward(&block[b * SHORT_MDCT..], &mut freq[b..], m, MAX_LM);
                }
            } else {
                self.mdct.forward(block, freq, 1, MAX_LM - lm);
            }
        }
        let mut band_e = [0.0f32; 2 * BANDS];
        let mut band_log_e = [0.0f32; 2 * BANDS];
        for channel in 0..c {
            let freq = &self.freq[channel * n..(channel + 1) * n];
            let x = &mut self.x[channel * n..(channel + 1) * n];
            x.fill(0.0);
            for band in start..end {
                let bins = m * EBANDS[band]..m * EBANDS[band + 1];
                let energy = (1e-27
                    + freq[bins.clone()]
                        .iter()
                        .map(|&value| value * value)
                        .sum::<f32>())
                .sqrt();
                band_e[channel * BANDS + band] = energy;
                band_log_e[channel * BANDS + band] = (energy.log2() - E_MEANS[band]).max(MIN_LOG_E);
                let gain = 1.0 / energy;
                for (out, &value) in x[bins.clone()].iter_mut().zip(&freq[bins]) {
                    *out = value * gain;
                }
            }
        }
        // The bandwidth of the audio: the bands above the last one with
        // anything to hear are let go.
        let mut bandwidth = start + 1;
        for band in start..end {
            let loudest = (0..c)
                .map(|channel| band_log_e[channel * BANDS + band] + E_MEANS[band])
                .fold(f32::MIN, f32::max);
            // About -90 dB of full scale in a bin.
            let bins = ((EBANDS[band + 1] - EBANDS[band]) << lm) as f32;
            if loudest - 0.5 * bins.log2() > 0.0 {
                bandwidth = band + 1;
            }
        }

        let bytes = match budget {
            Budget::Fixed(bytes) => bytes,
            Budget::Variable { average, max } if !silent => {
                // More for the short blocks, which code less well, and
                // less for audio that stops short of the top bands.
                let mut factor = if transient { 1.4 } else { 1.0 };
                let coded = EBANDS[bandwidth] as f32 / EBANDS[end] as f32;
                factor *= 0.5 + 0.5 * coded;
                ((average * factor).round() as usize).clamp(2, max)
            }
            Budget::Variable { .. } => 2,
        }
        .clamp(2, 1_275);
        let total_bits = (bytes * 8) as i32;

        let mut ec = RangeEncoder::new(bytes);
        if c == 1 {
            for band in 0..BANDS {
                self.old_band_e[band] = self.old_band_e[band].max(self.old_band_e[BANDS + band]);
            }
        }
        // The flags at the head of the frame, each coded only if the
        // decoder will read it, which it judges by `tell` as it last took
        // it.
        let mut tell = ec.tell();
        if tell == 1 {
            ec.bit_logp(silent, 15);
        } else {
            silent = false;
        }
        if silent {
            // The decoder reads nothing more, and forgets the energies.
            self.rng = ec.range();
            for channel in 0..2 {
                for band in 0..BANDS {
                    self.old_band_e[channel * BANDS + band] =
                        if band < end { MIN_LOG_E } else { 0.0 };
                }
            }
            self.started = true;
            return ec.finish().unwrap_or_else(|| vec![0; bytes]);
        }
        // No pitch pre-filter.
        if start == 0 && tell + 16 <= total_bits {
            ec.bit_logp(false, 1);
            tell = ec.tell();
        }
        if lm > 0 && tell + 3 <= total_bits {
            ec.bit_logp(transient, 3);
            tell = ec.tell();
        }

        // The coarse energy, intra or inter, whichever takes fewer bits.
        let mut error = [0.0f32; 2 * BANDS];
        let can_intra = tell + 3 <= total_bits;
        let mut best: Option<(RangeEncoder, [f32; 2 * BANDS], [f32; 2 * BANDS], bool)> = None;
        for intra in [false, true] {
            if intra && !can_intra {
                continue;
            }
            if !intra && !self.started && can_intra {
                // Nothing to predict from yet.
                continue;
            }
            let mut trial = ec.clone();
            if can_intra {
                trial.bit_logp(intra, 3);
            }
            let mut old = self.old_band_e;
            let mut err = [0.0f32; 2 * BANDS];
            coarse_energy(
                &mut trial,
                &mut old,
                &band_log_e,
                &mut err,
                start,
                end,
                intra,
                c,
                lm,
            );
            let better = match &best {
                None => true,
                Some((other, ..)) => trial.tell_frac() < other.tell_frac(),
            };
            if better {
                best = Some((trial, old, err, intra));
            }
        }
        let (coded, old, err, _intra) = best.expect("one of intra and inter is tried");
        ec = coded;
        self.old_band_e = old;
        error.copy_from_slice(&err);

        // Time-frequency resolution: long blocks as they are, short ones
        // recombined in the bands that are sparser so.
        let flags = if transient {
            self.tf_analysis(n, m, lm, start, end)
        } else {
            [0; BANDS]
        };
        let mut tf_res = [0i32; BANDS];
        tf_encode(&mut ec, start, end, transient, &flags, &mut tf_res, lm);

        let spread = if ec.tell() + 4 <= total_bits {
            ec.icdf(SPREAD_NORMAL, &SPREAD_ICDF, 5);
            SPREAD_NORMAL
        } else {
            SPREAD_NORMAL
        };

        let mut cap = [0i32; BANDS];
        caps(lm, c, &mut cap);

        // Bands that stand out from those around them, the peaks of tonal
        // audio, are boosted (dynalloc): more bits, and never skipped.
        let wanted = boosts(&band_log_e, c, start, end.min(bandwidth));
        let mut offsets = [0i32; BANDS];
        let mut dynalloc_logp = 6u32;
        let mut total_frac = total_bits << BITRES;
        let mut tell_frac = ec.tell_frac();
        // At most a quarter of the frame goes to boosts.
        let mut boost_room = total_frac / 4;
        for band in start..end {
            let width = ((c * (EBANDS[band + 1] - EBANDS[band])) << lm) as i32;
            let quanta = (width << BITRES).min((6 << BITRES).max(width));
            let mut loop_logp = dynalloc_logp;
            let mut boost = 0;
            let mut steps = 0;
            while tell_frac + ((loop_logp as i32) << BITRES) < total_frac && boost < cap[band] {
                let flag = steps < wanted[band] && quanta <= boost_room;
                ec.bit_logp(flag, loop_logp);
                tell_frac = ec.tell_frac();
                if !flag {
                    break;
                }
                boost += quanta;
                total_frac -= quanta;
                boost_room -= quanta;
                steps += 1;
                loop_logp = 1;
            }
            offsets[band] = boost;
            if boost > 0 {
                dynalloc_logp = (dynalloc_logp - 1).max(2);
            }
        }

        let alloc_trim = 5;
        if tell_frac + (6 << BITRES) <= total_frac {
            ec.icdf(alloc_trim as usize, &TRIM_ICDF, 7);
        }

        let mut bits = (((bytes * 8) as i32) << BITRES) - ec.tell_frac() - 1;
        let anti_collapse_rsv = if transient && lm >= 2 && bits >= ((lm as i32 + 2) << BITRES) {
            1 << BITRES
        } else {
            0
        };
        bits -= anti_collapse_rsv;

        let dual_stereo = c == 2 && choices.allow_dual && self.sparser_apart(n, m, choices);
        let alloc_choices = AllocChoices {
            intensity: choices.intensity,
            dual_stereo,
            bandwidth,
            depth: choices.depth,
        };
        let allocation = compute_allocation(
            start,
            end,
            &offsets,
            &cap,
            alloc_trim,
            bits,
            c,
            lm,
            &mut ec,
            &alloc_choices,
        );

        // The fine energy.
        for band in start..end {
            let fine = allocation.fine_quant[band];
            if fine <= 0 {
                continue;
            }
            for channel in 0..c {
                let at = band + channel * BANDS;
                let levels = 1i32 << fine;
                let q2 = (((error[at] + 0.5) * levels as f32).floor() as i32).clamp(0, levels - 1);
                ec.bits(q2 as u32, fine as u32);
                let offset = (q2 as f32 + 0.5) * (1 << (14 - fine)) as f32 * (1.0 / 16_384.0) - 0.5;
                self.old_band_e[at] += offset;
                error[at] -= offset;
            }
        }

        // The shapes.
        let mut collapse_masks = [0u8; 2 * BANDS];
        let frame = FrameBands {
            start,
            end,
            channels: c,
            lm,
            short_blocks: short,
            spread,
            dual_stereo: allocation.dual_stereo,
            intensity: allocation.intensity,
            tf_res: &tf_res,
            pulses: &allocation.pulses,
            total_bits: ((bytes * 8) as i32 * (1 << BITRES)) - anti_collapse_rsv,
            balance: allocation.balance,
            coded_bands: allocation.coded_bands,
        };
        if c == 1 {
            self.x[n..2 * n].fill(0.0);
        }
        quant_all_bands(
            &frame,
            &mut self.x[..2 * n],
            &mut collapse_masks,
            &mut ec,
            &mut self.rng,
            &mut self.bands,
            &band_e,
        );

        if anti_collapse_rsv > 0 {
            // Let the decoder fill short blocks that came out empty.
            ec.bits(1, 1);
        }

        // One more bit of fine energy where the bits left reach.
        let mut bits_left = (bytes * 8) as i32 - ec.tell();
        for priority in 0..2 {
            let mut band = start;
            while band < end && bits_left >= c as i32 {
                let fine = allocation.fine_quant[band];
                if fine >= MAX_FINE_BITS || allocation.fine_priority[band] != priority {
                    band += 1;
                    continue;
                }
                for channel in 0..c {
                    let at = band + channel * BANDS;
                    let q2 = u32::from(error[at] >= 0.0);
                    ec.bits(q2, 1);
                    let offset =
                        (q2 as f32 - 0.5) * (1 << (14 - fine - 1)) as f32 * (1.0 / 16_384.0);
                    self.old_band_e[at] += offset;
                    error[at] -= offset;
                    bits_left -= 1;
                }
                band += 1;
            }
        }

        if c == 1 {
            let (first, second) = self.old_band_e.split_at_mut(BANDS);
            second.copy_from_slice(first);
        }
        for channel in 0..2 {
            for band in (0..start).chain(end..BANDS) {
                self.old_band_e[channel * BANDS + band] = 0.0;
            }
        }
        self.rng = ec.range();
        self.started = true;
        let frame = ec.finish();
        debug_assert!(frame.is_some(), "a CELT frame overran its bytes");
        frame.unwrap_or_else(|| vec![0; bytes])
    }

    /// The resolution flag of each band of a frame of short blocks (section
    /// 4.3.4.5, with `tf_select` 0): 1 keeps the time resolution of the
    /// short blocks, 0 recombines them into the frequency resolution of one
    /// long block. Each band takes the one under which its normalised bins
    /// have the smaller sum of magnitudes, the sparser, which the pulses
    /// code with less error (the L1 criterion), with a cost for each
    /// change from band to band, which the flags pay for in bits: a path
    /// of least cost through the bands (Viterbi).
    fn tf_analysis(&self, n: usize, m: usize, lm: usize, start: usize, end: usize) -> [i32; BANDS] {
        const SWITCH: f32 = 0.3;
        let mut cost = [[0.0f32; 2]; BANDS];
        let mut tmp = [0.0f32; 22 << MAX_LM];
        for band in start..end {
            let (lo, hi) = (m * EBANDS[band], m * EBANDS[band + 1]);
            let width = hi - lo;
            for channel in 0..self.channels {
                let x = &self.x[channel * n + lo..channel * n + hi];
                cost[band][1] += x.iter().map(|value| value.abs()).sum::<f32>();
                let tmp = &mut tmp[..width];
                tmp.copy_from_slice(x);
                for k in 0..lm {
                    haar1(tmp, width >> k, 1 << k);
                }
                cost[band][0] += tmp.iter().map(|value| value.abs()).sum::<f32>();
            }
        }
        // The cheapest path, with where each state came from.
        let mut total = [0.0f32, SWITCH];
        let mut from = [[0usize; 2]; BANDS];
        for band in start..end {
            let mut next = [0.0f32; 2];
            for state in 0..2 {
                let stay = total[state];
                let switch = total[1 - state] + SWITCH;
                let (best, origin) = if stay <= switch {
                    (stay, state)
                } else {
                    (switch, 1 - state)
                };
                next[state] = best + cost[band][state];
                from[band][state] = origin;
            }
            total = next;
        }
        let mut flags = [0i32; BANDS];
        let mut state = usize::from(total[1] < total[0]);
        for band in (start..end).rev() {
            flags[band] = state as i32;
            state = from[band][state];
        }
        flags
    }

    /// Whether the two channels of the bands below intensity stereo are
    /// sparser coded apart than as mid and side: the sum of the magnitudes
    /// of their bins, a measure of how few pulses they need, is the
    /// smaller, by a margin.
    fn sparser_apart(&self, n: usize, m: usize, choices: &FrameChoices) -> bool {
        let top = m * EBANDS[choices.intensity.clamp(1, BANDS)];
        let (left, right) = self.freq.split_at(n);
        let mut apart = 1e-15f32;
        let mut together = 1e-15f32;
        for (&l, &r) in left[..top].iter().zip(&right[..top]) {
            apart += l.abs() + r.abs();
            together += std::f32::consts::FRAC_1_SQRT_2 * ((l + r).abs() + (l - r).abs());
        }
        together > 1.2 * apart
    }
}

/// How many steps of boost each band below `end` wants: one for each
/// 6 dB, past the first 12, that its level, the louder of its channels,
/// stands above the mean of the two bands on either side of it.
fn boosts(log_e: &[f32; 2 * BANDS], channels: usize, start: usize, end: usize) -> [usize; BANDS] {
    let mut level = [0.0f32; BANDS];
    for band in start..BANDS {
        level[band] = (0..channels)
            .map(|channel| log_e[channel * BANDS + band] + E_MEANS[band])
            .fold(f32::MIN, f32::max);
    }
    let mut wanted = [0usize; BANDS];
    for band in start..end {
        let around: Vec<f32> = (band.saturating_sub(2).max(start)..(band + 3).min(BANDS))
            .filter(|&other| other != band)
            .map(|other| level[other])
            .collect();
        if around.is_empty() {
            continue;
        }
        let mean = around.iter().sum::<f32>() / around.len() as f32;
        let excess = level[band] - mean;
        if excess > 2.0 {
            wanted[band] = ((excess - 1.0) as usize).min(3);
        }
    }
    wanted
}

/// Codes the coarse energy of every band (section 4.3.2.1): the
/// difference, rounded to whole steps of 6 dB, between each band's log
/// energy and its prediction from the frame before (`coef`) and from the
/// bands below (`prev`, with `beta`), as the decoder works it out. Leaves
/// the decoder's energies in `old` and what the steps miss in `error`.
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
fn coarse_energy(
    ec: &mut RangeEncoder,
    old: &mut [f32; 2 * BANDS],
    log_e: &[f32; 2 * BANDS],
    error: &mut [f32; 2 * BANDS],
    start: usize,
    end: usize,
    intra: bool,
    channels: usize,
    lm: usize,
) {
    let model = &E_PROB_MODEL[lm][usize::from(intra)];
    let mut prev = [0.0f32; 2];
    let (coef, beta) = if intra {
        (0.0, BETA_INTRA)
    } else {
        (PRED_COEF[lm], BETA_COEF[lm])
    };
    let budget = (ec.storage() * 8) as i32;
    for band in start..end {
        for c in 0..channels {
            let at = band + c * BANDS;
            let target = log_e[at];
            let before = old[at].max(-9.0);
            let wanted = (target - coef * before - prev[c] + 0.5).floor() as i32;
            // Never so far down that the energy cannot come back.
            let wanted = wanted.max(-28 - (coef * before + prev[c]).floor() as i32);
            let left = budget - ec.tell();
            let qi = if left >= 15 {
                let index = 2 * band.min(20);
                laplace_encode(
                    ec,
                    wanted,
                    u32::from(model[index]) << 7,
                    u32::from(model[index + 1]) << 6,
                )
            } else if left >= 2 {
                let qi = wanted.clamp(-1, 1);
                let symbol = match qi {
                    0 => 0,
                    -1 => 1,
                    _ => 2,
                };
                ec.icdf(symbol, &SMALL_ENERGY_ICDF, 2);
                qi
            } else if left >= 1 {
                let qi = wanted.clamp(-1, 0);
                ec.bit_logp(qi == -1, 1);
                qi
            } else {
                -1
            };
            let q = qi as f32;
            old[at] = before;
            let tmp = coef * old[at] + prev[c] + q;
            old[at] = tmp;
            prev[c] = prev[c] + q - beta * q;
            error[at] = target - old[at];
        }
    }
}

/// Writes `value` with the Laplace distribution the coarse energy is coded
/// with (section 4.3.2.1): `fs` is the probability of zero in 1/32768,
/// `decay` how fast the rest falls off, as `laplace_decode` reads them.
/// A value beyond what the distribution holds is held to the largest it
/// does; the value written comes back.
fn laplace_encode(ec: &mut RangeEncoder, value: i32, fs: u32, decay: u32) -> i32 {
    const MIN_P: u32 = 1;
    const N_MIN: u32 = 16;
    let mut fl = 0u32;
    let mut fs = fs;
    let mut coded = 0i32;
    if value != 0 {
        let magnitude = value.unsigned_abs();
        fl = fs;
        fs = (((32_768 - MIN_P * (2 * N_MIN) - fs) * (16_384 - decay)) >> 15) + MIN_P;
        let mut reached = 1u32;
        while fs > MIN_P && reached < magnitude {
            fs *= 2;
            fl += fs;
            fs = (((fs - 2 * MIN_P) * decay) >> 15) + MIN_P;
            reached += 1;
        }
        if fs <= MIN_P {
            // Every value beyond takes the least probability, each sign.
            let room = (32_768 - fl - 2 * MIN_P) / (2 * MIN_P);
            let further = (magnitude - reached).min(room);
            reached += further;
            fl += 2 * further * MIN_P;
            fs = MIN_P;
        }
        coded = reached as i32;
        if value > 0 {
            fl += fs;
        } else {
            coded = -coded;
        }
    }
    ec.encode_bin(fl, fl + fs, 15);
    coded
}

/// Codes the resolution flag of each band (section 4.3.4.5) as `tf_decode`
/// reads it, as changes from band to band, and gives the change of
/// resolution each band then makes. A flag the bits do not reach stays as
/// the one before.
fn tf_encode(
    ec: &mut RangeEncoder,
    start: usize,
    end: usize,
    transient: bool,
    flags: &[i32; BANDS],
    tf_res: &mut [i32; BANDS],
    lm: usize,
) {
    let mut budget = (ec.storage() * 8) as i32;
    let mut tell = ec.tell();
    let mut logp: u32 = if transient { 2 } else { 4 };
    let select_rsv = lm > 0 && tell + (logp as i32) < budget;
    budget -= i32::from(select_rsv);
    let mut changed = 0;
    let mut current = 0;
    for (slot, &wanted) in tf_res.iter_mut().zip(flags).take(end).skip(start) {
        if tell + logp as i32 <= budget {
            ec.bit_logp(wanted != current, logp);
            current = wanted;
            tell = ec.tell();
            changed |= current;
        }
        *slot = current;
        logp = if transient { 4 } else { 5 };
    }
    let row = &TF_SELECT[lm];
    let t = 4 * usize::from(transient);
    let select = 0;
    if select_rsv && row[t + changed as usize] != row[t + 2 + changed as usize] {
        ec.bit_logp(select != 0, 1);
    }
    for slot in tf_res.iter_mut().take(end).skip(start) {
        *slot = i32::from(row[t + 2 * select + *slot as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{CeltDecoder, CeltScratch};
    use super::*;
    use crate::opus::range::RangeDecoder;

    /// A frame's worth of a signal of several kinds, by channel.
    fn signal(kind: usize, channels: usize, frame: usize, at: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; channels * frame];
        for channel in 0..channels {
            for i in 0..frame {
                let t = (at + i) as f32;
                let value = match kind {
                    0 => 0.4 * (t * 0.031 * (channel + 1) as f32).sin(),
                    1 => {
                        let noise = ((at + i) * 7_919 + channel * 104_729) % 65_521;
                        (noise as f32 / 65_521.0 - 0.5) * 0.6
                    }
                    2 => {
                        if (at + i) % 2_400 < 24 {
                            0.9
                        } else {
                            0.01 * (t * 0.11).sin()
                        }
                    }
                    _ => 0.0,
                };
                out[channel * frame + i] = value;
            }
        }
        out
    }

    /// Every frame the encoder writes, the decoder reads to the end with
    /// the same range and the same energies: the two agree on every
    /// symbol, at every frame size, rate and kind of audio.
    #[test]
    fn the_decoder_reads_what_the_encoder_meant() {
        for channels in [1usize, 2] {
            for (frame, bytes) in [
                (960, 160),
                (960, 40),
                (960, 8),
                (480, 60),
                (240, 23),
                (120, 15),
                (960, 1_275),
                (120, 3),
            ] {
                for kind in 0..4 {
                    for (intensity, allow_dual) in [(BANDS, false), (10, true), (0, false)] {
                        let choices = FrameChoices {
                            intensity,
                            allow_dual,
                            depth: 8,
                        };
                        let mut encoder = CeltEncoder::new(channels);
                        let mut decoder = CeltDecoder::new(channels);
                        let mut scratch = CeltScratch::new();
                        let mut out = vec![0.0f32; channels * frame];
                        for index in 0..12 {
                            let input = signal(kind, channels, frame, index * frame);
                            let data =
                                encoder.encode(&input, frame, Budget::Fixed(bytes), &choices);
                            assert_eq!(data.len(), bytes);
                            let mut ec = RangeDecoder::new(&data);
                            decoder
                                .decode(&mut scratch, &mut ec, bytes, frame, &mut out, frame, false)
                                .unwrap();
                            let what = format!(
                                "{channels} channels, {frame} samples in {bytes} bytes, kind {kind}, intensity {intensity}, frame {index}"
                            );
                            assert_eq!(decoder.rng, encoder.final_range(), "{what}");
                            assert_eq!(decoder.old_band_e, encoder.old_band_e, "{what}");
                        }
                    }
                }
            }
        }
    }

    /// What comes out is what went in, `OVERLAP` samples later, as near as
    /// the bits allow: the signal-to-noise ratio grows with them.
    #[test]
    fn the_decoded_frames_follow_the_input() {
        // White noise has no shape to find: a bit and a half a sample is
        // some 5 dB.
        for (kind, channels, at_least) in [
            (0usize, 1usize, 40.0),
            (0, 2, 40.0),
            (1, 2, 4.0),
            (2, 1, 12.0),
        ] {
            let mut last = f64::MIN;
            for bytes in [40usize, 80, 160, 320] {
                let frame = 960;
                let choices = FrameChoices {
                    intensity: BANDS,
                    allow_dual: true,
                    depth: 8,
                };
                let mut encoder = CeltEncoder::new(channels);
                let mut decoder = CeltDecoder::new(channels);
                let mut scratch = CeltScratch::new();
                let frames = 30;
                let mut input = vec![Vec::new(); channels];
                let mut output = vec![Vec::new(); channels];
                let mut out = vec![0.0f32; channels * frame];
                for index in 0..frames {
                    let block = signal(kind, channels, frame, index * frame);
                    let data = encoder.encode(&block, frame, Budget::Fixed(bytes), &choices);
                    let mut ec = RangeDecoder::new(&data);
                    decoder
                        .decode(&mut scratch, &mut ec, bytes, frame, &mut out, frame, false)
                        .unwrap();
                    for channel in 0..channels {
                        input[channel].extend_from_slice(&block[channel * frame..][..frame]);
                        output[channel].extend_from_slice(&out[channel * frame..][..frame]);
                    }
                }
                let (mut signal_power, mut noise) = (0.0f64, 0.0f64);
                for channel in 0..channels {
                    for i in 2 * frame..(frames - 1) * frame {
                        let want = f64::from(input[channel][i]);
                        let got = f64::from(output[channel][i + OVERLAP]);
                        signal_power += want * want;
                        noise += (got - want) * (got - want);
                    }
                }
                let snr = 10.0 * (signal_power / noise.max(1e-30)).log10();
                println!(
                    "kind {kind}, {channels} channels, {} kbit/s: {snr:.1} dB",
                    bytes * 8 * 50 / 1_000
                );
                assert!(snr > last - 0.5, "kind {kind}: {snr:.1} after {last:.1}");
                last = snr;
            }
            assert!(last > at_least, "kind {kind}: {last:.1} dB");
        }
    }

    /// Clicks over a quiet tone take short blocks where they strike, and
    /// only there.
    #[test]
    fn attacks_take_short_blocks() {
        let frame = 960;
        let choices = FrameChoices {
            intensity: BANDS,
            allow_dual: false,
            depth: 8,
        };
        let mut encoder = CeltEncoder::new(1);
        let mut transients = Vec::new();
        for index in 0..20 {
            let input = signal(2, 1, frame, index * frame);
            let data = encoder.encode(&input, frame, Budget::Fixed(160), &choices);
            let mut ec = RangeDecoder::new(&data);
            assert!(!ec.bit_logp(15));
            assert!(!ec.bit_logp(1));
            transients.push(ec.bit_logp(3));
        }
        // A click every 2400 samples, two and a half frames.
        let count = transients.iter().filter(|&&t| t).count();
        assert!((7..=9).contains(&count), "{transients:?}");
    }

    #[test]
    fn the_laplace_code_reads_back_and_holds_its_ends() {
        for (fs, decay) in [
            (72 << 7, 127 << 6),
            (24 << 7, 179 << 6),
            (177 << 7, 11 << 6),
        ] {
            for value in -40..=40 {
                let mut ec = RangeEncoder::new(16);
                let coded = laplace_encode(&mut ec, value, fs, decay);
                assert!(coded == value || coded.abs() < value.abs());
                let data = ec.finish().unwrap();
                let mut dec = RangeDecoder::new(&data);
                assert_eq!(super::super::laplace_decode(&mut dec, fs, decay), coded);
            }
        }
    }
}
