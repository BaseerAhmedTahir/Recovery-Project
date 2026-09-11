//! CRC-32/ISO-HDLC, the variant PNG and ZIP both use.
//!
//! Written out rather than pulled in as a dependency: it is twenty lines, both
//! validators that need it need exactly this one variant, and SPEC.md
//! section 1.3 makes every added dependency something to justify rather than
//! something to reach for.

/// Lazily built lookup table for the reflected polynomial 0xEDB88320.
static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();

fn table() -> &'static [u32; 256] {
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        t
    })
}

/// CRC-32 of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_continue(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// CRC-32 over several slices without concatenating them first.
///
/// PNG checksums the chunk type and the chunk data as one run, and they are
/// not adjacent in a way that lets us take a single slice cheaply.
pub fn crc32_parts(parts: &[&[u8]]) -> u32 {
    let mut c = 0xFFFF_FFFF;
    for p in parts {
        c = crc32_continue(c, p);
    }
    c ^ 0xFFFF_FFFF
}

fn crc32_continue(mut c: u32, data: &[u8]) -> u32 {
    let t = table();
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    // The check value every CRC catalogue lists for CRC-32/ISO-HDLC.
    #[test]
    fn matches_the_published_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn splitting_the_input_does_not_change_the_result() {
        let whole = crc32(b"123456789");
        assert_eq!(crc32_parts(&[b"1234", b"5678", b"9"]), whole);
        assert_eq!(crc32_parts(&[b"123456789"]), whole);
    }

    // A real PNG IHDR chunk: type bytes followed by 13 bytes of data, with the
    // CRC the encoder wrote. Taken from a 1x1 8-bit RGB image.
    #[test]
    fn checks_a_real_png_ihdr_chunk() {
        let ty = b"IHDR";
        let data = [
            0, 0, 0, 1, // width 1
            0, 0, 0, 1, // height 1
            8, // bit depth
            2, // colour type: truecolour
            0, 0, 0, // compression, filter, interlace
        ];
        assert_eq!(crc32_parts(&[ty, &data]), 0x907753DE);
    }
}
