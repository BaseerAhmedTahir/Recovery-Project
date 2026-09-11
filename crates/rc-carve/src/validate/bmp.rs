//! BMP validator: read the size fields back and check they agree.
//!
//! `BM` is two bytes. That is a hit roughly every 64 KB of random data, so on
//! a 512 GiB drive a header-only BMP signature yields about eight million
//! candidates. It is the worst false-positive generator in the database and
//! the reason `signatures.json` marks it as needing a validator.
//!
//! There is no checksum and no footer to lean on. What there is: four
//! independent size fields that must agree with each other - the file size,
//! the pixel-data offset, the DIB header size, and the dimensions times the
//! bit depth. Random data satisfies all four together very rarely.

use super::{le16, le32, Outcome};

/// Every DIB header size that has ever been defined.
const DIB_SIZES: &[u32] = &[12, 16, 40, 52, 56, 64, 108, 124];

pub fn validate(d: &[u8]) -> Outcome {
    if d.len() < 18 {
        return Outcome::reject("shorter than a BMP file header plus a DIB size").truncated();
    }
    if &d[..2] != b"BM" {
        return Outcome::reject("does not begin with BM");
    }

    let file_size = le32(d, 2).unwrap_or(0);
    let reserved = le32(d, 6).unwrap_or(0);
    let data_offset = le32(d, 10).unwrap_or(0);
    let dib_size = le32(d, 14).unwrap_or(0);

    // Both reserved words are zero in every writer anyone has ever shipped.
    if reserved != 0 {
        return Outcome::reject("the reserved words are non-zero");
    }
    if !DIB_SIZES.contains(&dib_size) {
        return Outcome::reject(format!("DIB header size {dib_size} is not a defined value"));
    }
    let header_end = 14u32.saturating_add(dib_size);
    if data_offset < header_end {
        return Outcome::reject("pixel data starts inside the headers");
    }
    if file_size < data_offset {
        return Outcome::reject("the declared file size is smaller than the pixel-data offset");
    }

    // BITMAPCOREHEADER stores 16-bit dimensions; everything later uses 32-bit.
    let (width, height, planes, bpp) = if dib_size == 12 {
        (
            le16(d, 18).unwrap_or(0) as i64,
            le16(d, 20).unwrap_or(0) as i64,
            le16(d, 22).unwrap_or(0),
            le16(d, 24).unwrap_or(0),
        )
    } else {
        (
            le32(d, 18).unwrap_or(0) as i32 as i64,
            le32(d, 22).unwrap_or(0) as i32 as i64,
            le16(d, 26).unwrap_or(0),
            le16(d, 28).unwrap_or(0),
        )
    };

    if planes != 1 {
        return Outcome::reject(format!(
            "colour planes is {planes}; a BMP always declares 1"
        ));
    }
    if !matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32) {
        return Outcome::reject(format!("{bpp} bits per pixel is not a defined value"));
    }
    // A negative height means a top-down bitmap and is legal; zero is not, and
    // width is never negative.
    if width <= 0 || height == 0 {
        return Outcome::reject("a dimension is zero or negative");
    }
    let abs_h = height.unsigned_abs();

    // Rows are padded to a four-byte boundary. For an uncompressed bitmap this
    // gives an exact expected size, which is the strongest check available
    // here: it ties three fields together at once.
    let compression = if dib_size >= 40 {
        le32(d, 30).unwrap_or(0)
    } else {
        0
    };
    let row_bytes = (width as u64 * bpp as u64).div_ceil(32) * 4;
    let expected_pixels = row_bytes.saturating_mul(abs_h);

    if compression == 0 {
        let claimed = file_size as u64 - data_offset as u64;
        // Writers differ on whether trailing padding or colour tables are
        // counted, so allow the pixel array to be no smaller than the geometry
        // demands rather than demanding equality.
        if claimed < expected_pixels {
            return Outcome::reject(format!(
                "declares {claimed} bytes of pixel data but {width}x{abs_h} at {bpp}bpp needs {expected_pixels}"
            ));
        }
    }

    let evidence = |o: Outcome| {
        o.with("width", width)
            .with("height", abs_h)
            .with("bits_per_pixel", bpp)
            .with("compression", compression)
            .with("top_down", height < 0)
    };

    if (file_size as u64) > d.len() as u64 {
        // BMP is the one format here that still knows its own size when the
        // data is gone: the length lives in the file header, which survived.
        // So this is Partial - we do not have all of it - with an *established*
        // length, unlike a truncated JPEG whose real end is unknowable.
        return evidence(
            Outcome::partial(
                file_size as u64,
                format!(
                    "the header declares {file_size} bytes but only {} are available; \
                     truncated, though the declared size is still trustworthy",
                    d.len()
                ),
            )
            .established()
            .truncated(),
        );
    }

    evidence(Outcome::valid(file_size as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    fn bmp(width: i32, height: i32, bpp: u16) -> Vec<u8> {
        let row = ((width as u64) * bpp as u64).div_ceil(32) * 4;
        let pixels = row * height.unsigned_abs() as u64;
        let data_offset = 14u32 + 40;
        let file_size = data_offset as u64 + pixels;

        let mut v = b"BM".to_vec();
        v.extend_from_slice(&(file_size as u32).to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // reserved
        v.extend_from_slice(&data_offset.to_le_bytes());
        v.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
        v.extend_from_slice(&width.to_le_bytes());
        v.extend_from_slice(&height.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // planes
        v.extend_from_slice(&bpp.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        v.extend_from_slice(&(pixels as u32).to_le_bytes());
        v.extend_from_slice(&[0u8; 16]); // ppm, palette counts
        v.resize(file_size as usize, 0x7F);
        v
    }

    #[test]
    fn accepts_a_well_formed_bmp_and_reports_the_declared_size() {
        let b = bmp(8, 8, 24);
        let out = validate(&b);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, b.len() as u64);
        assert_eq!(out.evidence_of("width"), Some("8"));
        assert_eq!(out.evidence_of("bits_per_pixel"), Some("24"));
    }

    #[test]
    fn a_negative_height_is_a_legal_top_down_bitmap() {
        let out = validate(&bmp(4, -4, 32));
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.evidence_of("top_down"), Some("true"));
        assert_eq!(out.evidence_of("height"), Some("4"));
    }

    #[test]
    fn trailing_bytes_do_not_extend_the_length() {
        let mut b = bmp(4, 4, 24);
        let real = b.len();
        b.extend_from_slice(&[0xEE; 1024]);
        assert_eq!(validate(&b).length, real as u64);
    }

    /// The point of the validator: two matching bytes are not evidence.
    #[test]
    fn rejects_chance_bm_pairs_in_pseudorandom_data() {
        let mut rejected = 0;
        let mut total = 0;
        let mut state = 0x1234_5678u32;
        for _ in 0..2000 {
            let mut v = b"BM".to_vec();
            for _ in 0..64 {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
                v.push((state >> 16) as u8);
            }
            total += 1;
            if validate(&v).status == Status::Rejected {
                rejected += 1;
            }
        }
        assert_eq!(rejected, total, "every random BM must be rejected");
    }

    #[test]
    fn rejects_a_pixel_array_too_small_for_the_geometry() {
        let mut b = bmp(64, 64, 24);
        // Claim the same geometry in a file the size of a 4x4 image.
        let shrunk = 54u32 + 48;
        b[2..6].copy_from_slice(&shrunk.to_le_bytes());
        b.truncate(shrunk as usize);
        assert_eq!(validate(&b).status, Status::Rejected);
    }

    #[test]
    fn a_bmp_cut_short_keeps_the_size_its_header_declares() {
        let b = bmp(32, 32, 24);
        let out = validate(&b[..b.len() / 2]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        // Unlike the other formats, a truncated BMP still knows how big it was.
        assert_eq!(out.length, b.len() as u64);
        assert!(out.length_established, "the header size is trustworthy");
    }

    #[test]
    fn rejects_undefined_bit_depths_and_plane_counts() {
        let mut b = bmp(8, 8, 24);
        b[28..30].copy_from_slice(&7u16.to_le_bytes());
        assert_eq!(validate(&b).status, Status::Rejected);

        let mut b = bmp(8, 8, 24);
        b[26..28].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(validate(&b).status, Status::Rejected);
    }
}
