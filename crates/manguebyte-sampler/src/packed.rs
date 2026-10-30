//! Lossless compression of the frames a sample keeps in memory.
//!
//! A channel is cut into blocks of [`BLOCK`] frames, each packed on its own
//! so that any block can be unpacked without the ones before it (a voice
//! jumps round its loop, and starts anywhere). A block whose every value
//! is an integer over 2^15 or 2^23 (what a 16- or 24-bit file decodes to)
//! is held as those integers: the residual of the best of five fixed
//! polynomial predictors (orders 0 to 4, as FLAC's fixed predictors), in
//! Rice codes with a parameter per 256 frames. Any other block, and any
//! block whose values do not come back bit for bit, is held as its `f32`
//! bits. Unpacking gives back exactly the values packed.

/// Frames in a block.
pub(crate) const BLOCK: usize = 1_024;

/// Frames that share a Rice parameter.
const PARTITION: usize = 256;

/// Highest fixed predictor order.
const MAX_ORDER: usize = 4;

/// A quotient of this many ones or more is followed by the value in 32
/// bits instead.
const ESCAPE: u32 = 40;

/// What a block holds, in the low two bits of its first byte.
const RAW: u8 = 0;
const INT16: u8 = 1;
const INT24: u8 = 2;

/// One channel's frames, packed block by block.
#[derive(Debug, Clone)]
pub(crate) struct PackedChannel {
    /// Where each block starts in `bytes`, and where the last ends.
    offsets: Box<[u32]>,
    bytes: Box<[u8]>,
    frames: usize,
}

impl PackedChannel {
    /// Packs `samples`.
    pub(crate) fn pack(samples: &[f32]) -> Self {
        let mut bytes = Vec::new();
        let mut offsets = Vec::with_capacity(samples.len().div_ceil(BLOCK) + 1);
        for block in samples.chunks(BLOCK) {
            offsets.push(bytes.len() as u32);
            pack_block(block, &mut bytes);
        }
        offsets.push(bytes.len() as u32);
        Self {
            offsets: offsets.into(),
            bytes: bytes.into(),
            frames: samples.len(),
        }
    }

    /// Bytes it takes.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes.len() + self.offsets.len() * 4
    }

    /// Unpacks block `block` into `out` and returns its frames (fewer than
    /// [`BLOCK`] only for the last; 0 past the end). Never allocates.
    pub(crate) fn unpack(&self, block: usize, out: &mut [f32; BLOCK]) -> usize {
        let (Some(&start), Some(&end)) = (self.offsets.get(block), self.offsets.get(block + 1))
        else {
            return 0;
        };
        let frames = (self.frames - block * BLOCK).min(BLOCK);
        let data = self
            .bytes
            .get(start as usize..end as usize)
            .unwrap_or_default();
        unpack_block(data, &mut out[..frames]);
        frames
    }
}

/// The scale of a kind of integer block.
fn scale(kind: u8) -> f32 {
    if kind == INT16 { 32_768.0 } else { 8_388_608.0 }
}

/// The values of `block` as integers over the scale of `kind`, if every one
/// comes back bit for bit.
fn as_integers(block: &[f32], kind: u8, integers: &mut [i32]) -> bool {
    let scale = scale(kind);
    let inverse = 1.0 / scale;
    for (value, integer) in block.iter().zip(integers.iter_mut()) {
        let scaled = f64::from(*value) * f64::from(scale);
        if !scaled.is_finite() || scaled.abs() > f64::from(1u32 << 24) {
            return false;
        }
        let whole = scaled.round() as i32;
        if (whole as f32 * inverse).to_bits() != value.to_bits() {
            return false;
        }
        *integer = whole;
    }
    true
}

/// The prediction of fixed order `order` for frame `n` of `x`, from the
/// frames before it. Frames before the block's start use lower orders.
#[inline]
fn predict(x: &[i32], n: usize, order: usize) -> i64 {
    let at = |back: usize| i64::from(x[n - back]);
    match order.min(n) {
        0 => 0,
        1 => at(1),
        2 => 2 * at(1) - at(2),
        3 => 3 * at(1) - 3 * at(2) + at(3),
        _ => 4 * at(1) - 6 * at(2) + 4 * at(3) - at(4),
    }
}

fn zigzag(value: i64) -> u32 {
    ((value << 1) ^ (value >> 63)) as u32
}

fn unzigzag(value: u32) -> i64 {
    i64::from(value >> 1) ^ -i64::from(value & 1)
}

fn pack_block(block: &[f32], bytes: &mut Vec<u8>) {
    let mut integers = [0i32; BLOCK];
    let integers = &mut integers[..block.len()];
    let kind = if as_integers(block, INT16, integers) {
        INT16
    } else if as_integers(block, INT24, integers) {
        INT24
    } else {
        bytes.push(RAW);
        for value in block {
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        return;
    };
    // The order whose residuals are smallest.
    let order = (0..=MAX_ORDER)
        .min_by_key(|&order| {
            (0..integers.len())
                .map(|n| (i64::from(integers[n]) - predict(integers, n, order)).unsigned_abs())
                .sum::<u64>()
        })
        .unwrap_or(0);
    bytes.push(kind | (order as u8) << 2);
    let mut residuals = [0u32; BLOCK];
    for n in 0..integers.len() {
        residuals[n] = zigzag(i64::from(integers[n]) - predict(integers, n, order));
    }
    let mut writer = BitWriter::new(bytes);
    for partition in residuals[..integers.len()].chunks(PARTITION) {
        let sum: u64 = partition.iter().map(|&value| u64::from(value)).sum();
        let mut k = 0u32;
        while k < 30 && (partition.len() as u64) << (k + 1) <= sum {
            k += 1;
        }
        writer.write(k, 5);
        for &value in partition {
            let quotient = value >> k;
            if quotient < ESCAPE {
                writer.ones(quotient);
                writer.write(0, 1);
                writer.write(value & ((1 << k) - 1), k);
            } else {
                writer.ones(ESCAPE);
                writer.write(value, 32);
            }
        }
    }
    writer.finish();
}

fn unpack_block(data: &[u8], out: &mut [f32]) {
    let Some((&header, rest)) = data.split_first() else {
        out.fill(0.0);
        return;
    };
    let kind = header & 3;
    if kind == RAW {
        for (value, bytes) in out.iter_mut().zip(rest.chunks_exact(4)) {
            *value = f32::from_bits(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
        }
        return;
    }
    let order = usize::from(header >> 2).min(MAX_ORDER);
    let inverse = 1.0 / scale(kind);
    let mut integers = [0i32; BLOCK];
    let mut reader = BitReader::new(rest);
    let mut n = 0;
    while n < out.len() {
        let k = reader.read(5);
        let end = (n + PARTITION).min(out.len());
        while n < end {
            let quotient = reader.ones(ESCAPE);
            let value = if quotient < ESCAPE {
                (quotient << k) | reader.read(k)
            } else {
                reader.read(32)
            };
            let sample = unzigzag(value) + predict(&integers, n, order);
            integers[n] = sample as i32;
            out[n] = integers[n] as f32 * inverse;
            n += 1;
        }
    }
}

/// Writes bits, the first in the highest place.
struct BitWriter<'a> {
    bytes: &'a mut Vec<u8>,
    word: u64,
    used: u32,
}

impl<'a> BitWriter<'a> {
    fn new(bytes: &'a mut Vec<u8>) -> Self {
        Self {
            bytes,
            word: 0,
            used: 0,
        }
    }

    /// The low `count` bits of `value`, up to 32.
    fn write(&mut self, value: u32, count: u32) {
        if count == 0 {
            return;
        }
        let value = u64::from(value) & ((1u64 << count) - 1);
        self.word |= value << (64 - self.used - count);
        self.used += count;
        while self.used >= 8 {
            self.bytes.push((self.word >> 56) as u8);
            self.word <<= 8;
            self.used -= 8;
        }
    }

    fn ones(&mut self, mut count: u32) {
        while count > 0 {
            let run = count.min(24);
            self.write((1 << run) - 1, run);
            count -= run;
        }
    }

    fn finish(mut self) {
        if self.used > 0 {
            self.write(0, 8 - self.used);
        }
    }
}

/// Reads what [`BitWriter`] wrote; zeros past the end.
struct BitReader<'a> {
    bytes: &'a [u8],
    next: usize,
    word: u64,
    have: u32,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            next: 0,
            word: 0,
            have: 0,
        }
    }

    #[inline]
    fn refill(&mut self) {
        while self.have <= 56 {
            let byte = self.bytes.get(self.next).copied().unwrap_or(0);
            self.next += 1;
            self.word |= u64::from(byte) << (56 - self.have);
            self.have += 8;
        }
    }

    /// `count` bits, up to 32.
    #[inline]
    fn read(&mut self, count: u32) -> u32 {
        if count == 0 {
            return 0;
        }
        self.refill();
        let value = (self.word >> (64 - count)) as u32;
        self.word <<= count;
        self.have -= count;
        value
    }

    /// Counts ones up to `limit`, and takes the zero after them if there
    /// are fewer.
    #[inline]
    fn ones(&mut self, limit: u32) -> u32 {
        let mut count = 0;
        loop {
            self.refill();
            let run = (!self.word)
                .leading_zeros()
                .min(self.have)
                .min(limit - count);
            self.word <<= run;
            self.have -= run;
            count += run;
            if count == limit {
                return count;
            }
            if self.have > 0 {
                // The bit after the run is a zero.
                self.word <<= 1;
                self.have -= 1;
                return count;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(samples: &[f32]) -> PackedChannel {
        let packed = PackedChannel::pack(samples);
        let mut block = [0.0f32; BLOCK];
        let mut back = Vec::new();
        for index in 0..samples.len().div_ceil(BLOCK) {
            let frames = packed.unpack(index, &mut block);
            back.extend_from_slice(&block[..frames]);
        }
        assert_eq!(back.len(), samples.len());
        for (a, b) in back.iter().zip(samples) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
        packed
    }

    #[test]
    fn every_kind_of_block_comes_back_bit_for_bit() {
        let mut state = 1u64;
        let mut random = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 33) as u32
        };
        // 16-bit, 24-bit, float, full-scale steps, silence, odd lengths.
        let sixteen: Vec<f32> = (0..5_000)
            .map(|_| (random() % 65_536) as i32 - 32_768)
            .map(|v| v as f32 / 32_768.0)
            .collect();
        round_trip(&sixteen);
        let twenty_four: Vec<f32> = (0..3_001)
            .map(|n| ((n as f32 * 0.01).sin() * 8_000_000.0).round() / 8_388_608.0)
            .collect();
        round_trip(&twenty_four);
        let float: Vec<f32> = (0..2_000).map(|_| f32::from_bits(random())).collect();
        round_trip(&float);
        let steps: Vec<f32> = (0..2_048)
            .map(|n| {
                if n % 2 == 0 {
                    -1.0
                } else {
                    32_767.0 / 32_768.0
                }
            })
            .collect();
        round_trip(&steps);
        round_trip(&[0.0; 1_500]);
        round_trip(&[-0.0, 0.0, 1.0, -1.0]);
        round_trip(&[]);
        // Silence packs to about a bit a frame.
        let quiet = PackedChannel::pack(&[0.0; 4_096]);
        assert!(quiet.bytes() < 4_096 / 8 + 100, "{}", quiet.bytes());
    }

    /// Ten seconds of a pipe as a 16-bit file holds it: six harmonics of
    /// 110 Hz at 1/k, falling 3 dB a second, with noise 54 dB under the
    /// peak, at 48 kHz.
    fn pipe(bits: u32) -> Vec<f32> {
        let mut state = 7u64;
        let scale = f64::from(1u32 << (bits - 1));
        (0..480_000)
            .map(|n| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let noise = ((state >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2e-3;
                let t = f64::from(n) / 48_000.0;
                let mut value = 0.0;
                for k in 1..=6 {
                    value +=
                        (core::f64::consts::TAU * 110.0 * f64::from(k) * t).sin() / f64::from(k);
                }
                let value = 0.25 * value * (-t / 3.0).exp() + noise;
                // Plus zero: an integer decoder never gives -0.0.
                ((value * scale).round() / scale + 0.0) as f32
            })
            .collect()
    }

    #[test]
    fn a_pipe_packs_to_a_quarter_of_its_floats_at_16_bits() {
        for bits in [16, 24] {
            let samples = pipe(bits);
            let packed = round_trip(&samples);
            let ratio = (samples.len() * 4) as f64 / packed.bytes() as f64;
            let started = std::time::Instant::now();
            let mut block = [0.0f32; BLOCK];
            let mut total = 0.0f32;
            for index in 0..samples.len().div_ceil(BLOCK) {
                packed.unpack(index, &mut block);
                total += block[0];
            }
            let nanoseconds = started.elapsed().as_nanos() as f64 / samples.len() as f64;
            println!(
                "{bits}-bit pipe: {:.2} bits a sample, {ratio:.2} times smaller than f32, \
                 {:.2} times smaller than {bits}-bit, unpacked in {nanoseconds:.1} ns a frame \
                 ({})",
                packed.bytes() as f64 * 8.0 / samples.len() as f64,
                ratio * f64::from(bits) / 32.0,
                if cfg!(debug_assertions) {
                    "unoptimised build"
                } else {
                    "optimised build"
                },
            );
            assert!(total.is_finite());
            let expected = if bits == 16 { 4.0 } else { 1.9 };
            assert!(ratio > expected, "{bits} bits: {ratio}");
        }
    }
}
