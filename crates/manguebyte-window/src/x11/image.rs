//! The back buffer's pixels as `PutImage` sends them: plain code, with no
//! system call.

use manguebyte_gfx::IRect;

/// The bytes of a `PutImage` request before its pixels: its fixed part, and
/// the longer length field of the BIG-REQUESTS extension.
const PUT_IMAGE_HEADER: usize = 28;

/// How many whole rows of `row_bytes` fit in one `PutImage` request of at
/// most `max_request` bytes: at least one, as the protocol's smallest
/// limit (16 KiB) holds a row of 4000 pixels and every server that shows
/// windows wider than that takes far larger requests.
pub(super) fn rows_per_request(max_request: usize, row_bytes: usize) -> u32 {
    let rows = max_request.saturating_sub(PUT_IMAGE_HEADER) / row_bytes.max(1);
    u32::try_from(rows).unwrap_or(u32::MAX).max(1)
}

/// The rows of `rect` in `buffer`, a picture `width` pixels across with
/// four bytes to a pixel, one after another in `out` with nothing between
/// them. With `reversed`, each pixel's bytes are turned around: a server
/// that keeps the most significant byte first reads blue-green-red-alpha as
/// alpha-red-green-blue.
///
/// `rect` must lie inside the picture; what lies outside is left out.
pub(super) fn pack_rows(buffer: &[u8], width: u32, rect: IRect, reversed: bool, out: &mut Vec<u8>) {
    out.clear();
    let (Ok(x), Ok(y)) = (usize::try_from(rect.x), usize::try_from(rect.y)) else {
        return;
    };
    let stride = width as usize * 4;
    let bytes = rect.width as usize * 4;
    for row in 0..rect.height as usize {
        let from = (y + row) * stride + x * 4;
        let Some(pixels) = buffer.get(from..from + bytes) else {
            return;
        };
        out.extend_from_slice(pixels);
    }
    if reversed {
        for pixel in out.chunks_exact_mut(4) {
            pixel.reverse();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_hold_whole_rows_within_the_limit() {
        // 100 pixels a row: 400 bytes.
        assert_eq!(rows_per_request(28 + 400 * 3, 400), 3);
        assert_eq!(rows_per_request(28 + 400 * 3 - 1, 400), 2);
        // A row that does not fit still goes, and nothing divides by zero.
        assert_eq!(rows_per_request(0, 400), 1);
        assert_eq!(rows_per_request(1000, 0), 972);
        assert_eq!(rows_per_request(usize::MAX, 4), u32::MAX);
    }

    #[test]
    fn rows_are_packed_in_either_byte_order() {
        // Three by three, each pixel's bytes naming it.
        let buffer: Vec<u8> = (0..36).collect();
        let mut out = vec![99];
        pack_rows(&buffer, 3, IRect::new(1, 1, 2, 2), false, &mut out);
        assert_eq!(
            out,
            [
                16, 17, 18, 19, 20, 21, 22, 23, 28, 29, 30, 31, 32, 33, 34, 35
            ]
        );
        pack_rows(&buffer, 3, IRect::new(2, 0, 1, 2), true, &mut out);
        assert_eq!(out, [11, 10, 9, 8, 23, 22, 21, 20]);
        // Outside the picture there is nothing to send.
        pack_rows(&buffer, 3, IRect::new(-1, 0, 1, 1), false, &mut out);
        assert!(out.is_empty());
        pack_rows(&buffer, 3, IRect::new(0, 2, 1, 2), false, &mut out);
        assert_eq!(out, [24, 25, 26, 27]);
    }
}
