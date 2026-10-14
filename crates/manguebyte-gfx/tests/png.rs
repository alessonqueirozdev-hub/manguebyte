//! PNG encoding and decoding: exact round trips, every colour type, and
//! hostile input.
//!
//! The PNG files here are written by a small encoder in this file (stored,
//! uncompressed deflate blocks), so that each case states its bytes and no
//! other image library is involved.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use manguebyte_gfx::{Color, ImageError, MAX_SIDE, Pixmap, PremulColor};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

/// An allocator that records the largest single request made on a thread
/// while it is watched.
struct Recording;

impl Recording {
    fn record(size: usize) {
        // `try_with` because the allocator is still called while a thread's
        // locals are being torn down.
        let _ = WATCHING.try_with(|watching| {
            if watching.get() {
                let _ = LARGEST.try_with(|largest| largest.set(largest.get().max(size)));
            }
        });
    }
}

// SAFETY: every method forwards to the system allocator unchanged; the only
// addition is updating a thread-local maximum, which does not allocate.
unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::record(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record(new_size);
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

/// Runs `work` and returns the largest single allocation it asked for.
fn largest_allocation(work: impl FnOnce()) -> usize {
    LARGEST.with(|largest| largest.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    LARGEST.with(Cell::get)
}

/// CRC-32 as PNG uses it (ISO 3309, polynomial 0xEDB88320).
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        for &byte in *part {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

/// A zlib stream of stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(65_535).collect()
    };
    for (i, chunk) in chunks.iter().enumerate() {
        out.push(u8::from(i + 1 == chunks.len()));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// A PNG with the given header, extra chunks before the image data, and
/// rows of raw samples (each row is prefixed with filter type 0 here).
fn png(
    width: u32,
    height: u32,
    depth: u8,
    color_type: u8,
    extra: &[(&[u8; 4], Vec<u8>)],
    rows: &[Vec<u8>],
) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[depth, color_type, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    for (kind, data) in extra {
        chunk(&mut out, kind, data);
    }
    let mut raw = Vec::new();
    for row in rows {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn pixels(p: &Pixmap) -> Vec<[u8; 4]> {
    p.data()
        .chunks_exact(4)
        .map(|c| [c[0], c[1], c[2], c[3]])
        .collect()
}

#[test]
fn every_premultiplied_pixel_survives_a_png_round_trip() {
    // All 32,896 valid (value, alpha) pairs, in every channel arrangement
    // that matters: red carries the value, green its complement, blue half.
    let mut image = Pixmap::new(256, 256).expect("small");
    for alpha in 0..=255u32 {
        for value in 0..=255u32 {
            let v = value.min(alpha) as u8;
            let g = (255 - value).min(alpha) as u8;
            let b = (value / 2).min(alpha) as u8;
            let color = PremulColor::new(v, g, b, alpha as u8).expect("valid");
            image.set_pixel(value, alpha, color);
        }
    }
    let bytes = image.encode_png().expect("encodes");
    let back = Pixmap::decode_png(&bytes).expect("decodes");
    assert_eq!(back, image);
}

#[test]
fn grey_grey_with_alpha_and_rgb_are_expanded() {
    let grey = png(2, 1, 8, 0, &[], &[vec![0, 255]]);
    assert_eq!(
        pixels(&Pixmap::decode_png(&grey).expect("grey")),
        [[0, 0, 0, 255], [255, 255, 255, 255]]
    );
    // Grey 128 at alpha 64 is premultiplied to round(128 * 64 / 255) = 32.
    let grey_alpha = png(1, 1, 8, 4, &[], &[vec![128, 64]]);
    assert_eq!(
        pixels(&Pixmap::decode_png(&grey_alpha).expect("grey and alpha")),
        [[32, 32, 32, 64]]
    );
    let rgb = png(1, 2, 8, 2, &[], &[vec![10, 20, 30], vec![40, 50, 60]]);
    assert_eq!(
        pixels(&Pixmap::decode_png(&rgb).expect("rgb")),
        [[10, 20, 30, 255], [40, 50, 60, 255]]
    );
    // One bit per sample: 1 is white.
    let bits = png(3, 1, 1, 0, &[], &[vec![0b1010_0000]]);
    assert_eq!(
        pixels(&Pixmap::decode_png(&bits).expect("1-bit grey")),
        [[255, 255, 255, 255], [0, 0, 0, 255], [255, 255, 255, 255]]
    );
}

#[test]
fn sixteen_bit_samples_keep_their_high_byte() {
    let rgba16 = png(
        1,
        1,
        16,
        6,
        &[],
        &[vec![0x12, 0x34, 0xAB, 0xCD, 0x00, 0xFF, 0xFF, 0x00]],
    );
    assert_eq!(
        pixels(&Pixmap::decode_png(&rgba16).expect("16-bit")),
        [[0x12, 0xAB, 0x00, 0xFF]]
    );
}

#[test]
fn palettes_and_their_transparency_are_expanded() {
    let palette = vec![255, 0, 0, 0, 255, 0];
    let transparency = vec![255, 128];
    let indexed = png(
        2,
        1,
        8,
        3,
        &[(b"PLTE", palette), (b"tRNS", transparency)],
        &[vec![0, 1]],
    );
    assert_eq!(
        pixels(&Pixmap::decode_png(&indexed).expect("indexed")),
        [[255, 0, 0, 255], [0, 128, 0, 128]]
    );
}

#[test]
fn sizes_beyond_the_limits_are_refused() {
    let wide = png(MAX_SIDE + 1, 1, 8, 0, &[], &[]);
    assert_eq!(
        Pixmap::decode_png(&wide),
        Err(ImageError::TooLarge {
            width: MAX_SIDE + 1,
            height: 1
        })
    );
    // Within MAX_SIDE but more than 64 MiB of pixels: refused from the
    // header, before the decoder runs. (The decoder's own limit applies only
    // to the compressed data it buffers.)
    for (width, height) in [(4096, 4097), (MAX_SIDE, MAX_SIDE), (1, 16_777_217)] {
        let big = png(width, height, 8, 6, &[], &[]);
        assert_eq!(
            Pixmap::decode_png(&big),
            Err(ImageError::TooLarge { width, height })
        );
    }
    let zero = png(0, 5, 8, 0, &[], &[]);
    assert!(Pixmap::decode_png(&zero).is_err());
    assert_eq!(
        Pixmap::new(0, 3).expect("empty").encode_png(),
        Err(ImageError::Empty)
    );
}

#[test]
fn a_header_alone_never_makes_the_decoder_allocate_more_than_64_mib() {
    // A file of under 100 bytes: a header announcing an image, and image
    // data that is a valid zlib stream of nothing. The decoder sizes its
    // output from the header, so the header is what must be checked. The
    // largest image allowed takes exactly 64 MiB; one more row is refused
    // before anything is allocated for it.
    for (width, height, limit) in [
        (4096, 4097, 1 << 20),
        (MAX_SIDE, MAX_SIDE, 1 << 20),
        (4096, 4096, 64 << 20),
        (2048, 2048, 16 << 20),
    ] {
        let bytes = png(width, height, 8, 6, &[], &[]);
        assert!(bytes.len() < 100);
        let mut result = None;
        let largest = largest_allocation(|| result = Some(Pixmap::decode_png(&bytes)));
        assert!(matches!(result, Some(Err(_))), "{result:?}");
        assert!(
            largest <= limit,
            "a {}-byte file announcing {width}x{height} made the decoder allocate {largest} bytes",
            bytes.len()
        );
    }
}

#[test]
fn malformed_headers_are_errors() {
    for (depth, color_type) in [(3, 0), (8, 5), (16, 3), (2, 2), (0, 0)] {
        let bad = png(
            1,
            1,
            depth,
            color_type,
            &[],
            &[vec![0, 0, 0, 0, 0, 0, 0, 0]],
        );
        assert!(
            Pixmap::decode_png(&bad).is_err(),
            "depth {depth}, colour type {color_type}"
        );
    }
    // A palette image with no palette.
    assert!(Pixmap::decode_png(&png(1, 1, 8, 3, &[], &[vec![0]])).is_err());
    // Too few rows of data.
    assert!(Pixmap::decode_png(&png(2, 3, 8, 0, &[], &[vec![1, 2]])).is_err());
    // A chunk that claims 4 GiB.
    let mut huge = SIGNATURE.to_vec();
    huge.extend_from_slice(&u32::MAX.to_be_bytes());
    huge.extend_from_slice(b"IHDR");
    huge.extend_from_slice(&[0; 32]);
    assert!(Pixmap::decode_png(&huge).is_err());
    assert!(Pixmap::decode_png(&[]).is_err());
    assert!(Pixmap::decode_png(&SIGNATURE).is_err());
}

/// A small generator of pseudo-random numbers (xorshift64*), so the
/// hostile inputs are the same on every run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Decodes `bytes` and checks that the outcome is consistent: an error,
/// or pixels of a legal size that are validly premultiplied.
fn decode_consistently(bytes: &[u8]) -> bool {
    match Pixmap::decode_png(bytes) {
        Ok(p) => {
            assert!(p.width() <= MAX_SIDE && p.height() <= MAX_SIDE);
            assert_eq!(p.data().len(), p.width() as usize * p.height() as usize * 4);
            assert!(
                p.data()
                    .chunks_exact(4)
                    .all(|c| c[0] <= c[3] && c[1] <= c[3] && c[2] <= c[3])
            );
            true
        }
        Err(_) => false,
    }
}

/// The chunk boundaries of a well-formed PNG: (start of length, data
/// length).
fn chunks_of(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut at = 8;
    let mut out = Vec::new();
    while at + 12 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[at..at + 4].try_into().expect("4 bytes")) as usize;
        out.push((at, len));
        at += 12 + len;
    }
    out
}

#[test]
fn truncated_flipped_and_random_bytes_never_panic() {
    let mut source = Pixmap::new(7, 5).expect("small");
    for y in 0..5 {
        for x in 0..7 {
            let c = Color::rgba((x * 36) as u8, (y * 50) as u8, 99, (x * y * 6 + 30) as u8);
            source.set_pixel(x, y, c.premultiply());
        }
    }
    let valid = source.encode_png().expect("encodes");
    assert_eq!(Pixmap::decode_png(&valid).expect("decodes"), source);

    // Every truncation.
    for len in 0..valid.len() {
        decode_consistently(&valid[..len]);
    }

    // Every bit of every byte flipped, with the chunk's checksum fixed so
    // the decoder reads past it.
    let chunks = chunks_of(&valid);
    let mut decoded = 0;
    for i in 0..valid.len() {
        for bit in 0..8 {
            let mut bytes = valid.clone();
            bytes[i] ^= 1 << bit;
            if let Some(&(start, len)) = chunks
                .iter()
                .find(|&&(start, len)| i >= start + 4 && i < start + 8 + len)
            {
                let crc = crc32(&[&bytes[start + 4..start + 8 + len]]);
                bytes[start + 8 + len..start + 12 + len].copy_from_slice(&crc.to_be_bytes());
            }
            decoded += usize::from(decode_consistently(&bytes));
        }
    }
    // Some flips still decode (in pixel data, say); most do not.
    assert!(decoded > 0);

    // Random chunks with valid checksums after a valid header, and plain
    // random bytes.
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    for _ in 0..2_000 {
        let mut bytes = valid[..33].to_vec();
        for _ in 0..random.below(4) {
            let kinds: [&[u8; 4]; 6] = [b"IDAT", b"PLTE", b"tRNS", b"IEND", b"acTL", b"fcTL"];
            let kind = kinds[random.below(kinds.len())];
            let data: Vec<u8> = (0..random.below(64)).map(|_| random.next() as u8).collect();
            chunk(&mut bytes, kind, &data);
        }
        decode_consistently(&bytes);
        let noise: Vec<u8> = (0..random.below(300))
            .map(|_| random.next() as u8)
            .collect();
        decode_consistently(&noise);
        let mut signed = SIGNATURE.to_vec();
        signed.extend_from_slice(&noise);
        decode_consistently(&signed);
    }
}
