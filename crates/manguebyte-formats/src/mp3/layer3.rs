//! Decoding the audio of a layer III frame (ISO/IEC 11172-3, 2.4.3.4, and
//! ISO/IEC 13818-3 for the lower sample rates): side information,
//! scalefactors, Huffman decoding, requantisation, stereo processing,
//! reordering, alias reduction, the inverse MDCT, and the synthesis filter
//! bank.

use std::f64::consts::PI;
use std::sync::OnceLock;

use super::Mp3Error;
use super::bits::BitReader;
use super::header::{FrameHeader, Mode, Version};
use super::huffman::trees;
use super::synth::Synthesis;
use super::tables::{
    ALIAS, LONG_BANDS, LSF_BANDS, PRETAB, SCALEFACTOR_BITS, SHORT_BANDS, pair_table,
};

/// What the side information says about one channel of one granule.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct GranuleInfo {
    part2_3_length: usize,
    big_values: usize,
    global_gain: i32,
    scalefac_compress: u32,
    window_switching: bool,
    block_type: u8,
    mixed: bool,
    table_select: [u8; 3],
    subblock_gain: [u8; 3],
    region0_count: u8,
    region1_count: u8,
    preflag: bool,
    scalefac_scale: bool,
    count1_table_b: bool,
}

impl GranuleInfo {
    fn short(&self) -> bool {
        self.window_switching && self.block_type == 2
    }

    /// A granule of the block type given, for the encoder's tests of its
    /// filter bank against this one.
    #[cfg(test)]
    pub(super) fn of_block_type(block_type: u8) -> Self {
        Self {
            window_switching: block_type != 0,
            block_type,
            ..Self::default()
        }
    }
}

#[cfg(test)]
impl SideInfo {
    /// The block type of each granule of channel `channel`, for the
    /// encoder's tests.
    pub(super) fn block_types(&self, channel: usize) -> [u8; 2] {
        [0, 1].map(|granule| self.granules[granule][channel].block_type)
    }
}

/// The side information of a frame (2.4.1.7).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct SideInfo {
    /// How many bytes before this frame's own data its main data starts.
    pub(super) main_data_begin: usize,
    scfsi: [[bool; 4]; 2],
    granules: [[GranuleInfo; 2]; 2],
}

impl SideInfo {
    /// Bits of main data the frame's granules take, together.
    pub(super) fn main_data_bits(&self, header: &FrameHeader) -> usize {
        (0..header.granules())
            .flat_map(|granule| (0..header.channels()).map(move |channel| (granule, channel)))
            .map(|(granule, channel)| self.granules[granule][channel].part2_3_length)
            .sum()
    }
}

/// Reads the side information that follows the header and its CRC.
pub(super) fn side_info(header: &FrameHeader, bytes: &[u8]) -> Result<SideInfo, Mp3Error> {
    let malformed = || Mp3Error::Malformed("side information ends early");
    let mut bits = BitReader::new(bytes, 0, header.side_info_len() * 8);
    let channels = header.channels();
    let mpeg1 = header.version == Version::Mpeg1;
    let mut info = SideInfo {
        main_data_begin: bits.read(if mpeg1 { 9 } else { 8 }).ok_or_else(malformed)? as usize,
        ..SideInfo::default()
    };
    let private = match (mpeg1, channels) {
        (true, 1) => 5,
        (true, _) => 3,
        (false, 1) => 1,
        (false, _) => 2,
    };
    bits.read(private).ok_or_else(malformed)?;
    if mpeg1 {
        for scfsi in info.scfsi.iter_mut().take(channels) {
            for band in scfsi.iter_mut() {
                *band = bits.read(1).ok_or_else(malformed)? == 1;
            }
        }
    }
    for granule in 0..header.granules() {
        for channel in 0..channels {
            let mut read = |count| bits.read(count).ok_or_else(malformed);
            let mut gi = GranuleInfo {
                part2_3_length: read(12)? as usize,
                big_values: read(9)? as usize,
                global_gain: read(8)? as i32,
                scalefac_compress: read(if mpeg1 { 4 } else { 9 })?,
                window_switching: read(1)? == 1,
                ..GranuleInfo::default()
            };
            if gi.big_values > 288 {
                return Err(Mp3Error::Malformed("more big values than a granule holds"));
            }
            if gi.window_switching {
                gi.block_type = read(2)? as u8;
                gi.mixed = read(1)? == 1;
                for select in gi.table_select.iter_mut().take(2) {
                    *select = read(5)? as u8;
                }
                for gain in gi.subblock_gain.iter_mut() {
                    *gain = read(3)? as u8;
                }
                if gi.block_type == 0 {
                    return Err(Mp3Error::Malformed("window switching with a normal block"));
                }
                // Implicit: region 0 is eight short bands, or seven long
                // ones, and region 1 is the rest.
                gi.region0_count = if gi.block_type == 2 && !gi.mixed {
                    8
                } else {
                    7
                };
                gi.region1_count = 20 - gi.region0_count;
            } else {
                for select in gi.table_select.iter_mut() {
                    *select = read(5)? as u8;
                }
                gi.region0_count = read(4)? as u8;
                gi.region1_count = read(3)? as u8;
            }
            if mpeg1 {
                gi.preflag = read(1)? == 1;
            }
            gi.scalefac_scale = read(1)? == 1;
            gi.count1_table_b = read(1)? == 1;
            for &select in &gi.table_select {
                if select != 0 && pair_table(usize::from(select)).is_none() {
                    return Err(Mp3Error::Malformed("Huffman table 4 or 14 selected"));
                }
            }
            info.granules[granule][channel] = gi;
        }
    }
    Ok(info)
}

/// The scalefactors of one channel: 22 long bands, or 13 short bands of
/// three windows. In MPEG-2 also the largest value the scalefactor of each
/// band could have, which as an intensity position means none.
#[derive(Debug, Clone, Copy, Default)]
struct ScaleFactors {
    long: [u8; 22],
    short: [[u8; 3]; 13],
    long_max: [u8; 22],
    short_max: [u8; 13],
}

/// `|x|^(4/3)` for every value a Huffman code and its extra bits can give.
pub(super) fn power_table() -> &'static [f32] {
    static TABLE: OnceLock<Vec<f32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        (0..8_207)
            .map(|value| (f64::from(value)).powf(4.0 / 3.0) as f32)
            .collect()
    })
}

/// The tables of the inverse MDCT and its windows, which the encoder's
/// forward transform shares.
pub(super) struct Transform {
    /// `cos(pi / 72 (2i + 1 + 18)(2k + 1))` for 36 outputs and 18 inputs.
    pub(super) long: [[f32; 18]; 36],
    /// `cos(pi / 24 (2i + 1 + 6)(2k + 1))` for 12 outputs and 6 inputs.
    pub(super) short: [[f32; 6]; 12],
    /// The windows of block types 0, 1 and 3 (2.4.3.4.10.2), and of short
    /// blocks.
    pub(super) windows: [[f32; 36]; 4],
    pub(super) short_window: [f32; 12],
    /// The butterflies of alias reduction.
    pub(super) cs: [f32; 8],
    pub(super) ca: [f32; 8],
}

pub(super) fn transform() -> &'static Transform {
    static TRANSFORM: OnceLock<Box<Transform>> = OnceLock::new();
    TRANSFORM.get_or_init(|| {
        let mut t = Box::new(Transform {
            long: [[0.0; 18]; 36],
            short: [[0.0; 6]; 12],
            windows: [[0.0; 36]; 4],
            short_window: [0.0; 12],
            cs: [0.0; 8],
            ca: [0.0; 8],
        });
        for (i, row) in t.long.iter_mut().enumerate() {
            for (k, value) in row.iter_mut().enumerate() {
                *value = (PI / 72.0 * (2 * i + 1 + 18) as f64 * (2 * k + 1) as f64).cos() as f32;
            }
        }
        for (i, row) in t.short.iter_mut().enumerate() {
            for (k, value) in row.iter_mut().enumerate() {
                *value = (PI / 24.0 * (2 * i + 1 + 6) as f64 * (2 * k + 1) as f64).cos() as f32;
            }
        }
        let long = |i: usize| (PI / 36.0 * (i as f64 + 0.5)).sin();
        let short = |i: usize| (PI / 12.0 * (i as f64 + 0.5)).sin();
        for i in 0..36 {
            t.windows[0][i] = long(i) as f32;
            // A start block: long rising, flat, short falling, then zero.
            t.windows[1][i] = match i {
                0..=17 => long(i),
                18..=23 => 1.0,
                24..=29 => short(i - 18),
                _ => 0.0,
            } as f32;
            // A stop block: zero, short rising, flat, long falling.
            t.windows[3][i] = match i {
                0..=5 => 0.0,
                6..=11 => short(i - 6),
                12..=17 => 1.0,
                _ => long(i),
            } as f32;
        }
        for (i, value) in t.short_window.iter_mut().enumerate() {
            *value = short(i) as f32;
        }
        for (i, &c) in ALIAS.iter().enumerate() {
            let norm = (1.0 + c * c).sqrt();
            t.cs[i] = (1.0 / norm) as f32;
            t.ca[i] = (c / norm) as f32;
        }
        t
    })
}

/// The state of a decoder between frames, and its work space.
pub(super) struct Layer3 {
    /// The second half of the last inverse transform of each subband of each
    /// channel, to be added to the first half of the next.
    overlap: Box<[[[f32; 18]; 32]; 2]>,
    synthesis: [Synthesis; 2],
    scalefactors: [ScaleFactors; 2],
    values: Box<[i32; 576]>,
    spectra: Box<[[f32; 576]; 2]>,
    /// One past the last line of each channel's spectrum that is not zero.
    nonzero: [usize; 2],
    /// The samples of the frame decoded last.
    pub(super) pcm: Box<[[f32; 1_152]; 2]>,
}

impl Layer3 {
    pub(super) fn new() -> Self {
        Self {
            overlap: Box::new([[[0.0; 18]; 32]; 2]),
            synthesis: [Synthesis::new(), Synthesis::new()],
            scalefactors: [ScaleFactors::default(); 2],
            values: Box::new([0; 576]),
            spectra: Box::new([[0.0; 576]; 2]),
            nonzero: [0; 2],
            pcm: Box::new([[0.0; 1_152]; 2]),
        }
    }

    /// Forgets the state left by earlier frames, as at the start of a
    /// stream.
    pub(super) fn reset(&mut self) {
        *self.overlap = [[[0.0; 18]; 32]; 2];
        for synthesis in &mut self.synthesis {
            synthesis.reset();
        }
    }

    /// Decodes a frame whose main data starts at bit `start` of `main`, into
    /// [`pcm`](Self::pcm). With `main` empty the frame is decoded as silence:
    /// what earlier frames left to fade out still does.
    ///
    /// On an error the state is left as it is, half updated: the caller
    /// starts afresh.
    pub(super) fn decode(
        &mut self,
        header: &FrameHeader,
        side: &SideInfo,
        main: &[u8],
        start: usize,
    ) -> Result<(), Mp3Error> {
        let channels = header.channels();
        let silent = main.is_empty();
        let mut position = start;
        for granule in 0..header.granules() {
            for channel in 0..channels {
                let mut gi = side.granules[granule][channel];
                if silent {
                    self.spectra[channel] = [0.0; 576];
                    self.nonzero[channel] = 0;
                    continue;
                }
                let end = position + gi.part2_3_length;
                if end > main.len() * 8 {
                    return Err(Mp3Error::Malformed("a granule runs past the main data"));
                }
                let mut bits = BitReader::new(main, position, end);
                if header.version == Version::Mpeg1 {
                    self.read_scalefactors(&gi, side.scfsi[channel], granule, channel, &mut bits)?;
                } else {
                    let intensity_right = channel == 1
                        && header.mode == Mode::JointStereo
                        && header.mode_extension & 1 != 0;
                    gi.preflag =
                        self.read_lsf_scalefactors(&gi, intensity_right, channel, &mut bits)?;
                }
                self.nonzero[channel] =
                    decode_values(&gi, header.rate_index, &mut bits, &mut self.values)?;
                requantize(
                    &gi,
                    &self.scalefactors[channel],
                    header.rate_index,
                    &self.values,
                    self.nonzero[channel],
                    &mut self.spectra[channel],
                );
                position = end;
            }
            if channels == 2 && header.mode == Mode::JointStereo && !silent {
                self.stereo(header, &side.granules[granule]);
            }
            for channel in 0..channels {
                let gi = side.granules[granule][channel];
                let out = &mut self.pcm[channel][granule * 576..(granule + 1) * 576];
                let spectrum = &mut self.spectra[channel];
                if gi.short() {
                    reorder(spectrum, header.rate_index, gi.mixed);
                }
                reduce_aliases(spectrum, &gi);
                synthesize(
                    spectrum,
                    &gi,
                    &mut self.overlap[channel],
                    &mut self.synthesis[channel],
                    out,
                );
            }
        }
        Ok(())
    }

    /// Reads the scalefactors of an MPEG-1 granule (2.4.2.7). With `scfsi`
    /// set for a group of bands, granule 1 keeps the scalefactors granule 0
    /// read for it.
    fn read_scalefactors(
        &mut self,
        gi: &GranuleInfo,
        scfsi: [bool; 4],
        granule: usize,
        channel: usize,
        bits: &mut BitReader<'_>,
    ) -> Result<(), Mp3Error> {
        let malformed = || Mp3Error::Malformed("scalefactors run past the granule");
        let (slen1, slen2) = SCALEFACTOR_BITS[gi.scalefac_compress as usize];
        let sf = &mut self.scalefactors[channel];
        let mut read = |width: u8| {
            bits.read(u32::from(width))
                .ok_or_else(malformed)
                .map(|v| v as u8)
        };
        if gi.short() {
            let first = if gi.mixed {
                for band in 0..8 {
                    sf.long[band] = read(slen1)?;
                }
                3
            } else {
                0
            };
            for band in first..12 {
                let width = if band < 6 { slen1 } else { slen2 };
                for window in 0..3 {
                    sf.short[band][window] = read(width)?;
                }
            }
            sf.short[12] = [0; 3];
        } else {
            for (group, bands) in [0..6, 6..11, 11..16, 16..21].into_iter().enumerate() {
                if granule == 1 && scfsi[group] {
                    continue;
                }
                let width = if group < 2 { slen1 } else { slen2 };
                for band in bands {
                    sf.long[band] = read(width)?;
                }
            }
            sf.long[21] = 0;
        }
        Ok(())
    }

    /// Reads the scalefactors of an MPEG-2 granule (13818-3, 2.4.3.2), whose
    /// widths come from `scalefac_compress` in one of six ways.
    fn read_lsf_scalefactors(
        &mut self,
        gi: &GranuleInfo,
        intensity_right: bool,
        channel: usize,
        bits: &mut BitReader<'_>,
    ) -> Result<bool, Mp3Error> {
        let malformed = || Mp3Error::Malformed("scalefactors run past the granule");
        let compress = gi.scalefac_compress;
        let (widths, way, preflag) = if intensity_right {
            let value = compress >> 1;
            if value < 180 {
                ([value / 36, (value % 36) / 6, value % 6, 0], 3, false)
            } else if value < 244 {
                let value = value - 180;
                (
                    [(value & 63) >> 4, (value & 15) >> 2, value & 3, 0],
                    4,
                    false,
                )
            } else {
                let value = value - 244;
                ([value / 3, value % 3, 0, 0], 5, false)
            }
        } else if compress < 400 {
            (
                [
                    (compress >> 4) / 5,
                    (compress >> 4) % 5,
                    (compress & 15) >> 2,
                    compress & 3,
                ],
                0,
                false,
            )
        } else if compress < 500 {
            let value = compress - 400;
            ([(value >> 2) / 5, (value >> 2) % 5, value & 3, 0], 1, false)
        } else {
            let value = compress - 500;
            ([value / 3, value % 3, 0, 0], 2, true)
        };
        let kind = if gi.short() {
            if gi.mixed { 2 } else { 1 }
        } else {
            0
        };
        let counts = LSF_BANDS[way][kind];

        // The scalefactors in order, and the most each could be.
        let mut values = [0u8; 39];
        let mut maxima = [0u8; 39];
        let mut count = 0;
        for (&bands, &width) in counts.iter().zip(&widths) {
            for _ in 0..bands {
                values[count] = bits.read(width).ok_or_else(malformed)? as u8;
                maxima[count] = ((1u32 << width) - 1) as u8;
                count += 1;
            }
        }

        let sf = &mut self.scalefactors[channel];
        let mut next = 0;
        if gi.short() {
            let first = if gi.mixed {
                for band in 0..6 {
                    sf.long[band] = values[next];
                    sf.long_max[band] = maxima[next];
                    next += 1;
                }
                3
            } else {
                0
            };
            for band in first..12 {
                for window in 0..3 {
                    sf.short[band][window] = values[next];
                    next += 1;
                }
                sf.short_max[band] = maxima[next - 1];
            }
            sf.short[12] = [0; 3];
            sf.short_max[12] = sf.short_max[11];
        } else {
            sf.long[..21].copy_from_slice(&values[..21]);
            sf.long_max[..21].copy_from_slice(&maxima[..21]);
            sf.long[21] = 0;
            sf.long_max[21] = sf.long_max[20];
        }
        // Whether the upper bands are pre-emphasised, which MPEG-2 says here
        // and not with a flag of its own.
        Ok(preflag)
    }
}

impl Layer3 {
    /// Joint stereo (2.4.3.4.9, and 13818-3 for intensity at the lower
    /// rates): bands coded as mid and side are turned back into left and
    /// right, and bands the right channel codes only as an intensity
    /// position are made from the left channel.
    fn stereo(&mut self, header: &FrameHeader, granules: &[GranuleInfo; 2]) {
        let ms = header.mode_extension & 2 != 0;
        let intensity = header.mode_extension & 1 != 0;
        let [left, right] = &mut *self.spectra;
        if !intensity {
            if ms {
                let end = self.nonzero[0].max(self.nonzero[1]);
                mid_side(&mut left[..end], &mut right[..end]);
            }
            return;
        }

        let gi = &granules[1];
        let sf = &self.scalefactors[1];
        let rate = header.rate_index;
        let long = &LONG_BANDS[rate];
        let short = &SHORT_BANDS[rate];
        let mpeg1 = header.version == Version::Mpeg1;
        // The ratio between the two channels for an intensity position, or
        // `None` for the value that means no intensity coding.
        let scale = if gi.scalefac_compress & 1 == 1 {
            0.5f64
        } else {
            0.25
        };
        let ratios = |position: u8, maximum: u8| -> Option<(f32, f32)> {
            if mpeg1 {
                if position >= 7 {
                    return None;
                }
                let angle = f64::from(position) * PI / 12.0;
                let (sin, cos) = angle.sin_cos();
                Some(((sin / (sin + cos)) as f32, (cos / (sin + cos)) as f32))
            } else {
                if position == maximum {
                    return None;
                }
                let steps = f64::from(position.div_ceil(2));
                let factor = 2f64.powf(-scale * steps) as f32;
                Some(if position % 2 == 1 {
                    (factor, 1.0)
                } else {
                    (1.0, factor)
                })
            }
        };
        let apply = |left: &mut [f32], right: &mut [f32], ratio: Option<(f32, f32)>| match ratio {
            Some((to_left, to_right)) => {
                for (l, r) in left.iter_mut().zip(right.iter_mut()) {
                    let value = *l;
                    *l = value * to_left;
                    *r = value * to_right;
                }
            }
            None if ms => mid_side(left, right),
            None => {}
        };

        // Lines below 36 of a mixed block, and every line of a long one, are
        // in long bands.
        let long_end = if !gi.short() {
            576
        } else if gi.mixed {
            36
        } else {
            0
        };
        let first_short = (0..13)
            .find(|&band| 3 * usize::from(short[band]) >= long_end)
            .unwrap_or(13);
        let short_nonzero = (long_end..576).any(|line| right[line] != 0.0);
        if long_end > 0 {
            // The intensity bands are the ones above the last band in which
            // the right channel has a value, unless the short bands above
            // a mixed block's long ones have one.
            let last = if short_nonzero {
                Some(long_end - 1)
            } else {
                (0..long_end).rev().find(|&line| right[line] != 0.0)
            };
            let first = match last {
                None => 0,
                Some(line) => {
                    (0..22)
                        .find(|&band| usize::from(long[band + 1]) > line)
                        .unwrap_or(21)
                        + 1
                }
            };
            for band in 0..22 {
                let range = usize::from(long[band])..usize::from(long[band + 1]).min(long_end);
                if range.is_empty() {
                    continue;
                }
                let ratio = if band >= first {
                    // The last band has no scalefactor; it takes the one of
                    // the band below.
                    let known = band.min(20);
                    ratios(sf.long[known], sf.long_max[known])
                } else {
                    None
                };
                let (l, r) = (&mut left[range.clone()], &mut right[range]);
                if band >= first {
                    apply(l, r, ratio);
                } else if ms {
                    mid_side(l, r);
                }
            }
        }
        if gi.short() {
            for window in 0..3 {
                let lines = |band: usize| {
                    let width = usize::from(short[band + 1] - short[band]);
                    let start = 3 * usize::from(short[band]) + window * width;
                    start..start + width
                };
                let last = (first_short..13)
                    .rev()
                    .find(|&band| lines(band).any(|line| right[line] != 0.0));
                let first = last.map_or(first_short, |band| band + 1);
                for band in first_short..13 {
                    let range = lines(band);
                    let (l, r) = (&mut left[range.clone()], &mut right[range]);
                    if band >= first {
                        let known = band.min(11);
                        apply(l, r, ratios(sf.short[known][window], sf.short_max[known]));
                    } else if ms {
                        mid_side(l, r);
                    }
                }
            }
        }
    }
}

/// Turns mid and side back into left and right.
fn mid_side(left: &mut [f32], right: &mut [f32]) {
    let half = std::f32::consts::FRAC_1_SQRT_2;
    for (l, r) in left.iter_mut().zip(right.iter_mut()) {
        let (mid, side) = (*l, *r);
        *l = (mid + side) * half;
        *r = (mid - side) * half;
    }
}

/// Reads the Huffman-coded values of a granule (2.4.2.7, 2.4.3.4.4): pairs
/// in three regions with a table each, then quadruples of zeros and ones
/// until the granule's data runs out. Returns one past the last line that
/// was coded; the lines after it are zero.
fn decode_values(
    gi: &GranuleInfo,
    rate: usize,
    bits: &mut BitReader<'_>,
    values: &mut [i32; 576],
) -> Result<usize, Mp3Error> {
    let malformed = || Mp3Error::Malformed("big values run past the granule");
    let long = &LONG_BANDS[rate];
    let short = &SHORT_BANDS[rate];
    let big = (gi.big_values * 2).min(576);
    let (region1, region2) = if gi.window_switching {
        let start = if gi.block_type == 2 && !gi.mixed {
            3 * usize::from(short[3])
        } else {
            usize::from(long[8])
        };
        (start, 576)
    } else {
        let band = |index: usize| usize::from(long[index.min(22)]);
        let first = usize::from(gi.region0_count) + 1;
        (band(first), band(first + usize::from(gi.region1_count) + 1))
    };
    let trees = trees();
    let mut line = 0;
    while line < big {
        let region = if line < region1 {
            0
        } else if line < region2 {
            1
        } else {
            2
        };
        let number = usize::from(gi.table_select[region]);
        let (Some(table), Some(tree)) = (pair_table(number), trees.pairs[number].as_ref()) else {
            // Table 0: the region holds only zeros, and costs no bits.
            values[line] = 0;
            values[line + 1] = 0;
            line += 2;
            continue;
        };
        let pair = tree.decode(bits).ok_or_else(malformed)?;
        let mut value = |magnitude: usize| -> Result<i32, Mp3Error> {
            let mut magnitude = magnitude as i32;
            if table.linbits > 0 && magnitude == 15 {
                magnitude += bits.read(table.linbits).ok_or_else(malformed)? as i32;
            }
            if magnitude != 0 && bits.bit().ok_or_else(malformed)? == 1 {
                magnitude = -magnitude;
            }
            Ok(magnitude)
        };
        values[line] = value(pair / table.size)?;
        values[line + 1] = value(pair % table.size)?;
        line += 2;
    }

    // The quadruples. One that the granule's data ends in the middle of is
    // dropped, as some encoders write one.
    while line + 4 <= 576 && bits.left() > 0 {
        let quad = if gi.count1_table_b {
            bits.read(4).map(|code| 15 - code as usize)
        } else {
            trees.quad_a.decode(bits)
        };
        let Some(quad) = quad else {
            break;
        };
        let mut four = [
            ((quad >> 3) & 1) as i32,
            ((quad >> 2) & 1) as i32,
            ((quad >> 1) & 1) as i32,
            (quad & 1) as i32,
        ];
        let mut whole = true;
        for value in four.iter_mut().filter(|value| **value != 0) {
            match bits.bit() {
                Some(1) => *value = -1,
                Some(_) => {}
                None => {
                    whole = false;
                    break;
                }
            }
        }
        if !whole {
            break;
        }
        values[line..line + 4].copy_from_slice(&four);
        line += 4;
    }
    values[line..].fill(0);
    Ok(line)
}

/// `2^(quarters / 4)`, exactly where it is a power of two.
fn gain(quarters: i32) -> f32 {
    const FRACTIONS: [f32; 4] = [1.0, 1.189_207_1, std::f32::consts::SQRT_2, 1.681_792_8];
    2f32.powi(quarters.div_euclid(4)) * FRACTIONS[quarters.rem_euclid(4) as usize]
}

/// Turns the decoded values into the spectrum (2.4.3.4.7.1): each is
/// `sign(v) |v|^(4/3)` times a gain, in steps of a quarter of an octave,
/// that the global gain, the scalefactors and the subblock gains set.
fn requantize(
    gi: &GranuleInfo,
    sf: &ScaleFactors,
    rate: usize,
    values: &[i32; 576],
    end: usize,
    spectrum: &mut [f32; 576],
) {
    let powers = power_table();
    let shift = if gi.scalefac_scale { 4 } else { 2 };
    let base = gi.global_gain - 210;
    let long = &LONG_BANDS[rate];
    let short = &SHORT_BANDS[rate];
    spectrum.fill(0.0);
    let mut put = |line: usize, factor: f32| {
        let value = values[line];
        let magnitude = powers[value.unsigned_abs() as usize] * factor;
        spectrum[line] = if value < 0 { -magnitude } else { magnitude };
    };

    let long_end = if !gi.short() {
        576
    } else if gi.mixed {
        36
    } else {
        0
    };
    for band in 0..22 {
        let start = usize::from(long[band]);
        if start >= long_end.min(end) {
            break;
        }
        let pre = if gi.preflag {
            i32::from(PRETAB[band])
        } else {
            0
        };
        let factor = gain(base - shift * (i32::from(sf.long[band]) + pre));
        for line in start..usize::from(long[band + 1]).min(long_end).min(end) {
            put(line, factor);
        }
    }
    if gi.short() {
        let first = (0..13)
            .find(|&band| 3 * usize::from(short[band]) >= long_end)
            .unwrap_or(13);
        for band in first..13 {
            let width = usize::from(short[band + 1] - short[band]);
            let start = 3 * usize::from(short[band]);
            if start >= end {
                break;
            }
            for window in 0..3 {
                let quarters = base
                    - 8 * i32::from(gi.subblock_gain[window])
                    - shift * i32::from(sf.short[band][window]);
                let factor = gain(quarters);
                let from = start + window * width;
                for line in from..(from + width).min(end) {
                    put(line, factor);
                }
            }
        }
    }
}

/// Puts the values of short blocks, coded band by band and window by
/// window, in the order the inverse transform takes them: subband by
/// subband, the six lines of each window together (2.4.3.4.8).
pub(super) fn reorder(spectrum: &mut [f32; 576], rate: usize, mixed: bool) {
    let short = &SHORT_BANDS[rate];
    let first = if mixed {
        (0..13)
            .find(|&band| 3 * usize::from(short[band]) >= 36)
            .unwrap_or(13)
    } else {
        0
    };
    let source = *spectrum;
    for band in first..13 {
        let width = usize::from(short[band + 1] - short[band]);
        let start = 3 * usize::from(short[band]);
        for window in 0..3 {
            for j in 0..width {
                let frequency = usize::from(short[band]) + j;
                let target = (frequency / 6) * 18 + window * 6 + frequency % 6;
                spectrum[target] = source[start + window * width + j];
            }
        }
    }
}

/// Alias reduction (2.4.3.4.10.1): butterflies across each boundary between
/// subbands of long blocks. A short block has none; a mixed block has the
/// one between its two long subbands.
pub(super) fn reduce_aliases(spectrum: &mut [f32; 576], gi: &GranuleInfo) {
    let limit = match (gi.short(), gi.mixed) {
        (false, _) => 32,
        (true, true) => 2,
        (true, false) => return,
    };
    let t = transform();
    for subband in 1..limit {
        for i in 0..8 {
            let below = 18 * subband - 1 - i;
            let above = 18 * subband + i;
            let (a, b) = (spectrum[below], spectrum[above]);
            spectrum[below] = a * t.cs[i] - b * t.ca[i];
            spectrum[above] = b * t.cs[i] + a * t.ca[i];
        }
    }
}

/// The inverse MDCT of each subband, windowed and overlapped with the last
/// granule (2.4.3.4.10.2), the inversion of every other sample of the odd
/// subbands that the filter bank expects, and the filter bank: 576 samples.
pub(super) fn synthesize(
    spectrum: &[f32; 576],
    gi: &GranuleInfo,
    overlap: &mut [[f32; 18]; 32],
    synthesis: &mut Synthesis,
    out: &mut [f32],
) {
    let t = transform();
    let mut slots = [[0.0f32; 32]; 18];
    for (subband, saved) in overlap.iter_mut().enumerate() {
        let block_type = match (gi.window_switching, gi.mixed && subband < 2) {
            (false, _) | (true, true) => 0,
            (true, false) => gi.block_type,
        };
        let input = &spectrum[subband * 18..subband * 18 + 18];
        let mut z = [0.0f32; 36];
        if input.iter().all(|&value| value == 0.0) {
            // The upper subbands are often empty: what is left is the
            // overlap of the granule before.
        } else if block_type == 2 {
            for window in 0..3 {
                let coefficients = &input[window * 6..window * 6 + 6];
                for (i, row) in t.short.iter().enumerate() {
                    let y: f32 = row.iter().zip(coefficients).map(|(&c, &x)| c * x).sum();
                    z[6 + 6 * window + i] += y * t.short_window[i];
                }
            }
        } else {
            let window = &t.windows[usize::from(block_type)];
            for (i, row) in t.long.iter().enumerate() {
                let y: f32 = row.iter().zip(input).map(|(&c, &x)| c * x).sum();
                z[i] = y * window[i];
            }
        }
        for i in 0..18 {
            let mut value = z[i] + saved[i];
            saved[i] = z[i + 18];
            if subband % 2 == 1 && i % 2 == 1 {
                value = -value;
            }
            slots[i][subband] = value;
        }
    }
    for (slot, samples) in slots.iter().zip(out.chunks_exact_mut(32)) {
        synthesis.run(slot, samples);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A joint stereo header: MPEG-1 at 44.1 kHz, or MPEG-2 at 22.05 kHz,
    /// with the stereo coding `extension` names.
    fn joint_stereo(mpeg1: bool, extension: u8) -> FrameHeader {
        let second = if mpeg1 { 0xFB } else { 0xF3 };
        FrameHeader::parse([0xFF, second, 0x90, 0x40 | extension << 4]).unwrap()
    }

    fn assert_near(got: f32, want: f64, what: &str) {
        assert!(
            (f64::from(got) - want).abs() < 1e-6,
            "{what}: {got} against {want}"
        );
    }

    #[test]
    fn mid_and_side_become_left_and_right() {
        let mut decoder = Layer3::new();
        let mid = [1.0f32, 0.5, 0.0, -1.0];
        let side = [0.0f32, 0.5, 1.0, 1.0];
        decoder.spectra[0][..4].copy_from_slice(&mid);
        decoder.spectra[1][..4].copy_from_slice(&side);
        decoder.nonzero = [4, 4];
        decoder.stereo(&joint_stereo(true, 2), &[GranuleInfo::default(); 2]);
        let half = std::f64::consts::FRAC_1_SQRT_2;
        for i in 0..4 {
            let (m, s) = (f64::from(mid[i]), f64::from(side[i]));
            assert_near(decoder.spectra[0][i], (m + s) * half, "left");
            assert_near(decoder.spectra[1][i], (m - s) * half, "right");
        }
    }

    #[test]
    fn intensity_positions_split_the_left_channel_band_by_band() {
        // MPEG-1, long blocks. The right channel has values up to band 5,
        // so bands 6 and up are coded by intensity. Its scalefactors are
        // the positions; 7 means no intensity coding, and the last band,
        // which has no scalefactor, takes the position of the one below.
        let header = joint_stereo(true, 1);
        let bands = &LONG_BANDS[header.rate_index];
        let mut decoder = Layer3::new();
        decoder.spectra[0] = [1.0; 576];
        decoder.spectra[1] = [0.0; 576];
        decoder.spectra[1][usize::from(bands[5])] = 0.25;
        for band in 0..22 {
            decoder.scalefactors[1].long[band] = (band % 8) as u8;
        }
        decoder.stereo(&header, &[GranuleInfo::default(); 2]);
        for (band, &start) in bands.iter().enumerate().take(22) {
            let line = usize::from(start);
            let position = if band == 21 { 20 % 8 } else { band % 8 };
            let (left, right) = (decoder.spectra[0][line], decoder.spectra[1][line]);
            if band < 6 || position == 7 {
                assert_near(left, 1.0, "left, not intensity");
                let expected = if band == 5 { 0.25 } else { 0.0 };
                assert_near(right, expected, "right, not intensity");
            } else {
                // A ratio of tan(position pi / 12) between left and right.
                let ratio = (position as f64 * std::f64::consts::PI / 12.0).tan();
                assert_near(left, ratio / (1.0 + ratio), "left");
                assert_near(right, 1.0 / (1.0 + ratio), "right");
            }
        }
    }

    #[test]
    fn short_blocks_are_coded_by_intensity_window_by_window() {
        let header = joint_stereo(true, 1);
        let bands = &SHORT_BANDS[header.rate_index];
        let short = GranuleInfo {
            window_switching: true,
            block_type: 2,
            ..GranuleInfo::default()
        };
        let mut decoder = Layer3::new();
        decoder.spectra[0] = [1.0; 576];
        decoder.spectra[1] = [0.0; 576];
        // The right channel has a value in band 2 of window 0 and in band
        // 7 of window 2, and none in window 1.
        let line = |band: usize, window: usize| {
            let width = usize::from(bands[band + 1] - bands[band]);
            3 * usize::from(bands[band]) + window * width
        };
        decoder.spectra[1][line(2, 0)] = 0.5;
        decoder.spectra[1][line(7, 2)] = 0.5;
        for band in 0..13 {
            decoder.scalefactors[1].short[band] = [3, 0, 6];
        }
        decoder.stereo(&header, &[short, short]);
        let first = [3, 0, 8];
        let ratios = [3.0f64, 0.0, 6.0].map(|position| {
            let angle = position * std::f64::consts::PI / 12.0;
            let (sin, cos) = angle.sin_cos();
            (sin / (sin + cos), cos / (sin + cos))
        });
        for band in 0..13 {
            for window in 0..3 {
                let at = line(band, window);
                let (left, right) = (decoder.spectra[0][at], decoder.spectra[1][at]);
                if band >= first[window] {
                    assert_near(left, ratios[window].0, "left");
                    assert_near(right, ratios[window].1, "right");
                } else {
                    assert_near(left, 1.0, "left, not intensity");
                }
            }
        }
    }

    #[test]
    fn mpeg2_intensity_steps_by_a_power_of_two() {
        // Positions step by 2^(-1/4), or 2^(-1/2) when the right channel's
        // scalefactor_compress is odd; odd positions lower the left channel
        // and even ones the right, and the most a band's scalefactor could
        // hold means no intensity coding.
        for (compress, step) in [(0u32, 0.25f64), (1, 0.5)] {
            let header = joint_stereo(false, 1);
            let mut decoder = Layer3::new();
            decoder.spectra[0] = [1.0; 576];
            decoder.spectra[1] = [0.0; 576];
            let sf = &mut decoder.scalefactors[1];
            for band in 0..22 {
                sf.long[band] = band as u8;
                sf.long_max[band] = 15;
            }
            let right = GranuleInfo {
                scalefac_compress: compress,
                ..GranuleInfo::default()
            };
            decoder.stereo(&header, &[GranuleInfo::default(), right]);
            let bands = &LONG_BANDS[header.rate_index];
            for (band, &start) in bands.iter().enumerate().take(22) {
                let line = usize::from(start);
                let position = band.min(20);
                let (left, right) = (decoder.spectra[0][line], decoder.spectra[1][line]);
                if position == 15 {
                    assert_near(left, 1.0, "left, not intensity");
                    assert_near(right, 0.0, "right, not intensity");
                    continue;
                }
                let factor = 2f64.powf(-step * position.div_ceil(2) as f64);
                let (want_left, want_right) = if position % 2 == 1 {
                    (factor, 1.0)
                } else {
                    (1.0, factor)
                };
                assert_near(left, want_left, "left");
                assert_near(right, want_right, "right");
            }
        }
    }

    #[test]
    fn short_blocks_are_reordered_for_the_transform() {
        let rate = 0;
        let bands = &SHORT_BANDS[rate];
        let mut spectrum = [0.0f32; 576];
        // Each value says where it comes from: window and frequency.
        for band in 0..13 {
            let width = usize::from(bands[band + 1] - bands[band]);
            for window in 0..3 {
                for j in 0..width {
                    let frequency = usize::from(bands[band]) + j;
                    spectrum[3 * usize::from(bands[band]) + window * width + j] =
                        (window * 1_000 + frequency) as f32;
                }
            }
        }
        let original = spectrum;
        reorder(&mut spectrum, rate, false);
        for subband in 0..32 {
            for window in 0..3 {
                for j in 0..6 {
                    let value = spectrum[subband * 18 + window * 6 + j];
                    assert_eq!(value, (window * 1_000 + subband * 6 + j) as f32);
                }
            }
        }
        // In a mixed block the two long subbands stay where they are.
        let mut mixed = original;
        reorder(&mut mixed, rate, true);
        assert_eq!(mixed[..36], original[..36]);
        assert_eq!(mixed[36..], spectrum[36..]);
    }
}
