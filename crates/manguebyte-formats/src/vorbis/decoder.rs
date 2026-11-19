//! Decoding audio packets into samples (section 4.3 of the Vorbis I
//! specification).

use std::f64::consts::PI;

use super::VorbisError;
use super::bits::{BitReader, ilog};
use super::codebook::Codebook;
use super::mdct::Imdct;
use super::setup::{Floor1, Identification, Mode, Residue, Setup};

/// What a floor of type 1 turns into a gain: `10^(7 (i - 255) / 256)`, a
/// step of 35/64 dB from -139.45 dB to 0 dB. This is the table of section
/// 10.1 of the specification, which lists these values to eight figures.
pub(super) fn inverse_db_table() -> [f32; 256] {
    let mut table = [0.0f32; 256];
    for (index, value) in table.iter_mut().enumerate() {
        *value = 10f64.powf(7.0 * (index as f64 - 255.0) / 256.0) as f32;
    }
    table
}

/// The rising half of the Vorbis window over `len` samples (section 4.3.1):
/// `sin(pi/2 sin^2((i + 1/2) / len pi/2))`.
pub(super) fn slope(len: usize) -> Vec<f32> {
    (0..len)
        .map(|i| {
            let inner = ((i as f64 + 0.5) / len as f64 * PI / 2.0).sin();
            (PI / 2.0 * inner * inner).sin() as f32
        })
        .collect()
}

/// The state of one channel between packets.
#[derive(Debug)]
struct Channel {
    /// The second half of the last block, windowed, to be added to the
    /// first half of the next.
    overlap: Vec<f32>,
    /// The samples the last packet finished.
    pcm: Vec<f32>,
}

/// Decodes the audio packets of one stream.
#[derive(Debug)]
pub(crate) struct Decoder {
    ident: Identification,
    setup: Setup,
    mode_bits: u32,
    imdct: [Imdct; 2],
    slopes: [Vec<f32>; 2],
    inverse_db: [f32; 256],
    channels: Vec<Channel>,
    /// The spectrum of every channel, the halves of a long block one after
    /// the other.
    spectra: Vec<f32>,
    /// The size of the last block decoded, or `None` if there is none to
    /// overlap with: before the first packet, and after a [`reset`](Self::reset).
    previous: Option<usize>,
    /// How many samples of `pcm` the last packet finished.
    finished: usize,
    // Work space, kept so that decoding does not allocate. The floors are
    // kept as their points, `points` a channel, and drawn one channel at a
    // time into `curve`, and each channel's block goes through `block`: a
    // stream of many channels needs three long blocks' worth of memory for
    // each, not five.
    points: usize,
    floor_y: Vec<i32>,
    curve: Vec<f32>,
    block: Vec<f32>,
    final_y: Vec<i64>,
    step2: Vec<bool>,
    floor_used: Vec<bool>,
    no_residue: Vec<bool>,
    bundle: Vec<usize>,
    bundle_skip: Vec<bool>,
    offsets: Vec<usize>,
    classifications: Vec<u8>,
    interleaved: Vec<f32>,
}

impl Decoder {
    pub(crate) fn new(ident: Identification, setup: Setup) -> Self {
        let [short, long] = ident.block_sizes;
        let channels = (0..ident.channels)
            .map(|_| Channel {
                overlap: vec![0.0; long / 2],
                pcm: vec![0.0; long / 2],
            })
            .collect();
        let points = setup
            .floors
            .iter()
            .flatten()
            .map(|floor| floor.x.len())
            .max()
            .unwrap_or(0);
        Self {
            mode_bits: ilog(setup.modes.len() as u32 - 1),
            imdct: [Imdct::new(short), Imdct::new(long)],
            slopes: [slope(short / 2), slope(long / 2)],
            inverse_db: inverse_db_table(),
            channels,
            spectra: vec![0.0; ident.channels * long / 2],
            previous: None,
            finished: 0,
            points,
            floor_y: vec![0; points * ident.channels],
            curve: vec![0.0; long / 2],
            block: vec![0.0; long],
            final_y: vec![0; points],
            step2: vec![false; points],
            floor_used: vec![false; ident.channels],
            no_residue: vec![false; ident.channels],
            bundle: Vec::with_capacity(ident.channels),
            bundle_skip: Vec::with_capacity(ident.channels),
            offsets: Vec::with_capacity(ident.channels),
            classifications: Vec::new(),
            interleaved: Vec::new(),
            ident,
            setup,
        }
    }

    /// Forgets the last block, so that the next packet only starts a new
    /// run and finishes no samples.
    pub(crate) fn reset(&mut self) {
        self.previous = None;
        self.finished = 0;
    }

    /// The samples of `channel` the last packet finished.
    pub(crate) fn pcm(&self, channel: usize) -> &[f32] {
        &self.channels[channel].pcm[..self.finished]
    }

    /// The mode of an audio packet, from its first bits, and the reader
    /// past them. `None` for a packet that is not one, which decoding steps
    /// over.
    fn mode<'a>(&self, packet: &'a [u8]) -> Option<(Mode, BitReader<'a>)> {
        let mut bits = BitReader::new(packet);
        if bits.read(1)? != 0 {
            return None;
        }
        let mode = *self.setup.modes.get(bits.read(self.mode_bits)? as usize)?;
        Some((mode, bits))
    }

    /// The size of the block an audio packet holds, from its first bits.
    /// `None` for a packet that is not one, which decoding steps over.
    pub(crate) fn block_size(&self, packet: &[u8]) -> Option<usize> {
        let (mode, _) = self.mode(packet)?;
        Some(self.ident.block_sizes[usize::from(mode.long)])
    }

    /// Decodes an audio packet. Returns how many samples it finished, which
    /// are then in [`pcm`](Self::pcm), or `None` for a packet that is not
    /// one and was stepped over.
    pub(crate) fn decode(&mut self, packet: &[u8]) -> Result<Option<usize>, VorbisError> {
        let Some((mode, mut bits)) = self.mode(packet) else {
            return Ok(None);
        };
        let long = mode.long;
        let n = self.ident.block_sizes[usize::from(long)];
        // A packet that ends here still has its block; it holds silence.
        let (previous_long, next_long) = if long {
            (bits.flag().unwrap_or(false), bits.flag().unwrap_or(false))
        } else {
            (false, false)
        };
        let half = n / 2;
        let mapping = &self.setup.mappings[usize::from(mode.mapping)];
        let stride = self.ident.block_sizes[1] / 2;

        // The floor of each channel: its points for now.
        let points = self.points;
        for channel in 0..self.ident.channels {
            let submap = usize::from(mapping.mux[channel]);
            let floor_index = usize::from(mapping.submaps[submap].0);
            let Some(floor) = &self.setup.floors[floor_index] else {
                return Err(VorbisError::Unsupported("floors of type 0"));
            };
            let y = &mut self.floor_y[channel * points..(channel + 1) * points];
            let used = decode_floor1(floor, &self.setup.codebooks, &mut bits, y);
            self.floor_used[channel] = used;
            self.no_residue[channel] = !used;
        }

        // A channel coupled with one that is not silent has its residue
        // decoded, even if its own floor is unused.
        for &(magnitude, angle) in &mapping.coupling {
            let (magnitude, angle) = (usize::from(magnitude), usize::from(angle));
            if !self.no_residue[magnitude] || !self.no_residue[angle] {
                self.no_residue[magnitude] = false;
                self.no_residue[angle] = false;
            }
        }

        // The residues, submap by submap.
        self.spectra.fill(0.0);
        for (submap, &(_, residue)) in mapping.submaps.iter().enumerate() {
            self.bundle.clear();
            self.bundle_skip.clear();
            for channel in 0..self.ident.channels {
                if usize::from(mapping.mux[channel]) == submap {
                    self.bundle.push(channel);
                    self.bundle_skip.push(self.no_residue[channel]);
                }
            }
            let residue = &self.setup.residues[usize::from(residue)];
            decode_residue(
                residue,
                &self.setup.codebooks,
                &mut bits,
                half,
                stride,
                &self.bundle,
                &self.bundle_skip,
                &mut self.spectra,
                &mut self.offsets,
                &mut self.classifications,
                &mut self.interleaved,
            );
        }

        // Undo the coupling, last step first (section 1.3.3).
        for &(magnitude, angle) in mapping.coupling.iter().rev() {
            let (magnitude, angle) = (usize::from(magnitude), usize::from(angle));
            let (low, high, swapped) = if magnitude < angle {
                (magnitude, angle, false)
            } else {
                (angle, magnitude, true)
            };
            let (head, tail) = self.spectra.split_at_mut(high * stride);
            let first = &mut head[low * stride..low * stride + half];
            let second = &mut tail[..half];
            let (magnitudes, angles) = if swapped {
                (second, first)
            } else {
                (first, second)
            };
            for (m, a) in magnitudes.iter_mut().zip(angles.iter_mut()) {
                let (old_m, old_a) = (*m, *a);
                let (new_m, new_a) = if old_m > 0.0 {
                    if old_a > 0.0 {
                        (old_m, old_m - old_a)
                    } else {
                        (old_m + old_a, old_m)
                    }
                } else if old_a > 0.0 {
                    (old_m, old_m + old_a)
                } else {
                    (old_m - old_a, old_m)
                };
                *m = new_m;
                *a = new_a;
            }
        }

        // The spectrum of each channel is its floor times its residue; then
        // back to time, through the window, and the first half of the block
        // overlapped with the second half of the last one. What is finished
        // runs from the middle of the last block to the middle of this one.
        let size = usize::from(long);
        let (left, right) = self.window_bounds(n, long, previous_long, next_long);
        self.finished = self.previous.map_or(0, |previous| previous / 4 + half / 2);
        // Where this block starts, from the middle of the last one.
        let start = self
            .previous
            .map_or(0, |previous| (previous / 4) as isize - (half / 2) as isize);
        for (index, channel) in self.channels.iter_mut().enumerate() {
            let spectrum = &mut self.spectra[index * stride..index * stride + half];
            if self.floor_used[index] {
                let submap = usize::from(mapping.mux[index]);
                let floor_index = usize::from(mapping.submaps[submap].0);
                if let Some(floor) = &self.setup.floors[floor_index] {
                    render_floor1(
                        floor,
                        &self.floor_y[index * points..(index + 1) * points],
                        &mut self.final_y,
                        &mut self.step2,
                        &self.inverse_db,
                        &mut self.curve[..half],
                    );
                }
                for (value, &gain) in spectrum.iter_mut().zip(&self.curve[..half]) {
                    *value *= gain;
                }
            } else {
                spectrum.fill(0.0);
            }
            let block = &mut self.block[..n];
            self.imdct[size].inverse(spectrum, block);
            apply_window(block, left, right, &self.slopes);

            for (j, out) in channel.pcm[..self.finished].iter_mut().enumerate() {
                let earlier = channel.overlap.get(j).copied().unwrap_or(0.0);
                let at = j as isize - start;
                let later = if at >= 0 { block[at as usize] } else { 0.0 };
                *out = earlier + later;
            }
            channel.overlap[..half].copy_from_slice(&block[half..n]);
            channel.overlap[half..].fill(0.0);
        }
        self.previous = Some(n);
        Ok(Some(self.finished))
    }

    /// Where the rising and the falling slope of a block's window lie
    /// (section 4.3.1): for each, its start and which slope it is (0 short,
    /// 1 long). A long block next to a short one rises or falls with the
    /// short slope, around its quarter.
    fn window_bounds(
        &self,
        n: usize,
        long: bool,
        previous_long: bool,
        next_long: bool,
    ) -> ((usize, usize), (usize, usize)) {
        let short = self.ident.block_sizes[0];
        let left = if long && !previous_long {
            (n / 4 - short / 4, 0)
        } else {
            (0, usize::from(long))
        };
        let right = if long && !next_long {
            (n * 3 / 4 - short / 4, 0)
        } else {
            (n / 2, usize::from(long))
        };
        (left, right)
    }
}

/// Multiplies a block by its window: zero before the rising slope, the
/// slope, one up to the falling slope, the slope backwards, zero after.
fn apply_window(
    block: &mut [f32],
    left: (usize, usize),
    right: (usize, usize),
    slopes: &[Vec<f32>; 2],
) {
    let (left_start, left_slope) = left;
    let rise = &slopes[left_slope];
    block[..left_start].fill(0.0);
    for (value, &gain) in block[left_start..left_start + rise.len()]
        .iter_mut()
        .zip(rise)
    {
        *value *= gain;
    }
    let (right_start, right_slope) = right;
    let fall = &slopes[right_slope];
    for (value, &gain) in block[right_start..right_start + fall.len()]
        .iter_mut()
        .zip(fall.iter().rev())
    {
        *value *= gain;
    }
    block[right_start + fall.len()..].fill(0.0);
}

/// Reads the points of a floor of type 1 (section 7.2.3). Returns whether
/// the floor is used: an unused floor, or a packet that ends before the
/// floor does, leaves the channel silent.
fn decode_floor1(
    floor: &Floor1,
    books: &[Codebook],
    bits: &mut BitReader<'_>,
    y: &mut [i32],
) -> bool {
    let mut read = || -> Option<()> {
        if !bits.flag()? {
            return None;
        }
        let range = [256u32, 128, 86, 64][usize::from(floor.multiplier) - 1];
        let width = ilog(range - 1);
        y[0] = bits.read(width)? as i32;
        y[1] = bits.read(width)? as i32;
        let mut offset = 2;
        for &class in &floor.partition_classes {
            let class = usize::from(class);
            let dimensions = usize::from(floor.class_dimensions[class]);
            let sub_bits = u32::from(floor.class_subclasses[class]);
            let mask = (1u32 << sub_bits) - 1;
            let mut value = if sub_bits > 0 {
                books[usize::from(floor.class_masterbooks[class])].decode_scalar(bits)?
            } else {
                0
            };
            for _ in 0..dimensions {
                let book = floor.subclass_books[class][(value & mask) as usize];
                value >>= sub_bits;
                y[offset] = match book {
                    Some(book) => books[usize::from(book)].decode_scalar(bits)? as i32,
                    None => 0,
                };
                offset += 1;
            }
        }
        Some(())
    };
    read().is_some()
}

/// The Y of the line from `(x0, y0)` to `(x1, y1)` at `x` (section 9.2.6).
fn render_point(x0: i64, y0: i64, x1: i64, y1: i64, x: i64) -> i64 {
    let dy = y1 - y0;
    let adx = x1 - x0;
    let offset = dy.abs() * (x - x0) / adx;
    if dy < 0 { y0 - offset } else { y0 + offset }
}

/// Draws the line from `(x0, y0)` up to `x1` into `out`, through the table
/// of gains, with the integer steps of section 9.2.7.
fn render_line(x0: i64, y0: i64, x1: i64, y1: i64, out: &mut [f32], table: &[f32; 256]) {
    let dy = y1 - y0;
    let adx = x1 - x0;
    let base = dy / adx;
    let step = if dy < 0 { base - 1 } else { base + 1 };
    let ady = dy.abs() - base.abs() * adx;
    // A stream an encoder wrote stays within the table; one that does not
    // is held to it.
    let gain = |y: i64| table[y.clamp(0, 255) as usize];
    let mut y = y0;
    let mut error = 0;
    let len = out.len() as i64;
    if x0 < len {
        out[x0 as usize] = gain(y);
    }
    for x in x0 + 1..x1.min(len) {
        error += ady;
        if error >= adx {
            error -= adx;
            y += step;
        } else {
            y += base;
        }
        out[x as usize] = gain(y);
    }
}

/// Works out the curve of a floor of type 1 from its points (section 7.2.4)
/// and writes it into `out` as gains.
pub(super) fn render_floor1(
    floor: &Floor1,
    y: &[i32],
    final_y: &mut [i64],
    step2: &mut [bool],
    table: &[f32; 256],
    out: &mut [f32],
) {
    let range = [256, 128, 86, 64][usize::from(floor.multiplier) - 1];
    let x = |index: usize| i64::from(floor.x[index]);
    let points = floor.x.len();
    // Wide enough that no stream can make these overflow: each point adds
    // at most an entry number, of 24 bits.
    final_y[0] = i64::from(y[0]);
    final_y[1] = i64::from(y[1]);
    step2[0] = true;
    step2[1] = true;
    for point in 2..points {
        let low = usize::from(floor.low_neighbour[point]);
        let high = usize::from(floor.high_neighbour[point]);
        let predicted = render_point(x(low), final_y[low], x(high), final_y[high], x(point));
        let value = i64::from(y[point]);
        let high_room = range - predicted;
        let low_room = predicted;
        let room = if high_room < low_room {
            high_room * 2
        } else {
            low_room * 2
        };
        if value != 0 {
            step2[low] = true;
            step2[high] = true;
            step2[point] = true;
            final_y[point] = if value >= room {
                if high_room > low_room {
                    value - low_room + predicted
                } else {
                    predicted - value + high_room - 1
                }
            } else if value % 2 == 1 {
                predicted - (value + 1) / 2
            } else {
                predicted + value / 2
            };
        } else {
            step2[point] = false;
            final_y[point] = predicted;
        }
    }

    let multiplier = i64::from(floor.multiplier);
    let len = out.len() as i64;
    let (mut low_x, mut low_y) = (0, final_y[0] * multiplier);
    let (mut high_x, mut high_y) = (0, 0);
    for &point in &floor.sorted[1..] {
        let point = usize::from(point);
        if step2[point] {
            high_y = final_y[point] * multiplier;
            high_x = x(point);
            render_line(low_x, low_y, high_x, high_y, out, table);
            low_x = high_x;
            low_y = high_y;
        }
    }
    if high_x < len {
        render_line(high_x, high_y, len, high_y, out, table);
    }
}

/// Decodes a residue into the spectra of the channels of a submap
/// (section 8.6.2).
///
/// `bundle` lists the channels, `skip` which of them have nothing to
/// decode. Each channel's spectrum is `half` values at `channel * stride`
/// in `spectra`. A packet that ends early leaves the rest at zero.
#[allow(clippy::too_many_arguments)]
fn decode_residue(
    residue: &Residue,
    books: &[Codebook],
    bits: &mut BitReader<'_>,
    half: usize,
    stride: usize,
    bundle: &[usize],
    skip: &[bool],
    spectra: &mut [f32],
    offsets: &mut Vec<usize>,
    classifications: &mut Vec<u8>,
    interleaved: &mut Vec<f32>,
) {
    if residue.kind == 2 {
        // All channels as one vector, sample by sample, if any of them has
        // something to decode.
        if skip.iter().all(|&skip| skip) {
            return;
        }
        let channels = bundle.len();
        interleaved.clear();
        interleaved.resize(half * channels, 0.0);
        let vectors = Vectors {
            values: interleaved,
            offsets: &[0],
            len: half * channels,
        };
        decode_partitions(residue, 1, books, bits, vectors, &[false], classifications);
        for (slot, &channel) in bundle.iter().enumerate() {
            let spectrum = &mut spectra[channel * stride..channel * stride + half];
            for (index, value) in spectrum.iter_mut().enumerate() {
                *value = interleaved[index * channels + slot];
            }
        }
        return;
    }
    offsets.clear();
    offsets.extend(bundle.iter().map(|&channel| channel * stride));
    let vectors = Vectors {
        values: spectra,
        offsets,
        len: half,
    };
    decode_partitions(
        residue,
        residue.kind,
        books,
        bits,
        vectors,
        skip,
        classifications,
    );
}

/// The vectors a residue decodes into: `len` values at each of `offsets`.
struct Vectors<'a> {
    values: &'a mut [f32],
    offsets: &'a [usize],
    len: usize,
}

impl Vectors<'_> {
    fn get(&mut self, vector: usize) -> &mut [f32] {
        let start = self.offsets[vector];
        &mut self.values[start..start + self.len]
    }
}

/// The partitioned decoding that the three residue types share, with the
/// values of each vector laid out as in format 0 or format 1.
fn decode_partitions(
    residue: &Residue,
    format: u16,
    books: &[Codebook],
    bits: &mut BitReader<'_>,
    mut vectors: Vectors<'_>,
    skip: &[bool],
    classifications: &mut Vec<u8>,
) {
    let size = vectors.len;
    let begin = residue.begin.min(size);
    let end = residue.end.min(size);
    let partition_size = residue.partition_size;
    let partitions = end.saturating_sub(begin) / partition_size;
    if partitions == 0 {
        return;
    }
    let classbook = &books[usize::from(residue.classbook)];
    let words = classbook.dimensions;
    let row = partitions + words;
    classifications.clear();
    classifications.resize(row * skip.len(), 0);
    let count = residue.classifications as u32;

    let mut decode = || -> Option<()> {
        for pass in 0..8 {
            let mut partition = 0;
            while partition < partitions {
                if pass == 0 {
                    for (channel, _) in skip.iter().enumerate().filter(|(_, skip)| !**skip) {
                        let mut word = classbook.decode_scalar(bits)?;
                        for digit in (0..words).rev() {
                            classifications[channel * row + partition + digit] =
                                (word % count) as u8;
                            word /= count;
                        }
                    }
                }
                for _ in 0..words {
                    if partition >= partitions {
                        break;
                    }
                    for (channel, _) in skip.iter().enumerate().filter(|(_, skip)| !**skip) {
                        let class = usize::from(classifications[channel * row + partition]);
                        let Some(book) = residue.books[class][pass] else {
                            continue;
                        };
                        let book = &books[usize::from(book)];
                        let offset = begin + partition * partition_size;
                        let vector = vectors.get(channel);
                        if format == 0 {
                            // The values of a vector are spread across the
                            // partition, a step apart.
                            let step = partition_size / book.dimensions;
                            for i in 0..step {
                                let values = book.decode_vector(bits)?;
                                for (j, &value) in values.iter().enumerate() {
                                    vector[offset + i + j * step] += value;
                                }
                            }
                        } else {
                            // One after the other. A vector that runs past
                            // the partition runs on into the next, as the
                            // specification has it.
                            let mut i = 0;
                            while i < partition_size {
                                let values = book.decode_vector(bits)?;
                                for &value in values {
                                    if let Some(slot) = vector.get_mut(offset + i) {
                                        *slot += value;
                                    }
                                    i += 1;
                                }
                            }
                        }
                    }
                    partition += 1;
                }
            }
        }
        Some(())
    };
    let _ = decode();
}
