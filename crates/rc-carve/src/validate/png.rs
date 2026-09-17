//! PNG validator: chunk walk with CRC verification.
//!
//! PNG is the easiest format here to validate honestly, because it checksums
//! itself. Every chunk carries a CRC-32 over its type and data, so a chunk
//! that passes is a chunk that was recovered intact - not merely a chunk whose
//! length field happened to be plausible.
//!
//! That makes PNG the one format where "byte-exact recovery" can be asserted
//! from the file's own contents rather than from a fixture manifest, and it
//! makes the distinction between GREEN and YELLOW in SPEC.md section 5.7
//! a measured fact: a structurally complete PNG with a failing CRC has been
//! partially overwritten, and we can say which chunk.

use super::{be32, Outcome};
use crate::crc32::crc32_parts;

pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The spec caps a chunk length at 2^31 - 1, and the high bit being set is the
/// usual sign of a length field read out of noise.
const MAX_CHUNK: u32 = 0x7FFF_FFFF;

pub fn validate(d: &[u8]) -> Outcome {
    if d.len() < SIGNATURE.len() {
        return Outcome::reject("shorter than the PNG signature").truncated();
    }
    if d[..8] != SIGNATURE {
        return Outcome::reject("signature mismatch");
    }

    let mut i = 8usize;
    let mut first = true;
    let mut saw_idat = false;
    let mut chunks = 0u64;
    let mut crc_errors = 0u64;
    let mut first_bad_chunk = String::new();
    let mut first_bad_at = 0usize;
    let mut dims = (0u32, 0u32);
    let mut bit_depth = 0u8;
    let mut colour_type = 0u8;

    loop {
        let Some(len) = be32(d, i) else {
            return truncated(i, saw_idat, "ran out of data at a chunk length");
        };
        if len > MAX_CHUNK {
            return structural(i, saw_idat, "a chunk length has its high bit set");
        }
        let len = len as usize;
        let Some(ty) = d.get(i + 4..i + 8) else {
            return truncated(i, saw_idat, "ran out of data at a chunk type");
        };
        // A chunk type is four ASCII letters; the case of each carries meaning
        // but anything outside A-Za-z is not a chunk at all.
        if !ty.iter().all(|b| b.is_ascii_alphabetic()) {
            return structural(i, saw_idat, "a chunk type is not four ASCII letters");
        }
        let ty_name = String::from_utf8_lossy(ty).to_string();

        // length + type + data + crc
        let Some(end) = i.checked_add(12).and_then(|v| v.checked_add(len)) else {
            return structural(i, saw_idat, "chunk length overflows the offset");
        };
        if end > d.len() {
            return truncated(i, saw_idat, "a chunk extends past the available data");
        }

        if first {
            if ty_name != "IHDR" || len != 13 {
                return Outcome::reject("the first chunk is not a 13-byte IHDR");
            }
            let w = be32(d, i + 8).unwrap_or(0);
            let h = be32(d, i + 12).unwrap_or(0);
            bit_depth = d[i + 16];
            colour_type = d[i + 17];
            let compression = d[i + 18];
            let filter = d[i + 19];
            let interlace = d[i + 20];
            if w == 0 || h == 0 {
                return Outcome::reject("IHDR declares a zero dimension");
            }
            if !matches!(bit_depth, 1 | 2 | 4 | 8 | 16) {
                return Outcome::reject(format!("IHDR declares bit depth {bit_depth}"));
            }
            if !matches!(colour_type, 0 | 2 | 3 | 4 | 6) {
                return Outcome::reject(format!("IHDR declares colour type {colour_type}"));
            }
            // Only one compression method and one filter method have ever been
            // defined, so anything else is noise rather than a future PNG.
            if compression != 0 || filter != 0 || interlace > 1 {
                return Outcome::reject("IHDR declares undefined compression, filter or interlace");
            }
            dims = (w, h);
            first = false;
        }

        // The CRC covers the type and the data, but not the length.
        let want = be32(d, i + 8 + len).unwrap_or(0);
        let got = crc32_parts(&[ty, &d[i + 8..i + 8 + len]]);
        if want != got {
            crc_errors += 1;
            if first_bad_chunk.is_empty() {
                first_bad_chunk = format!("{ty_name}@{i}");
                first_bad_at = i;
            }
            // A bad IHDR CRC means we cannot trust the dimensions we just read,
            // and IHDR is the one chunk whose contents we act on.
            if ty_name == "IHDR" {
                return Outcome::reject("the IHDR checksum fails; this is not an intact PNG");
            }
        }

        if ty_name == "IDAT" {
            saw_idat = true;
        }
        chunks += 1;
        i = end;

        if ty_name == "IEND" {
            if !saw_idat {
                return Outcome::reject("reached IEND with no image data");
            }
            let out = Outcome::valid(i as u64)
                .with("width", dims.0)
                .with("height", dims.1)
                .with("bit_depth", bit_depth)
                .with("colour_type", colour_type)
                .with("chunks", chunks)
                .with("crc_errors", crc_errors);
            return if crc_errors > 0 {
                // Structurally whole, demonstrably damaged. This is precisely
                // the YELLOW case, and we can name the chunk.
                Outcome::partial(
                    i as u64,
                    format!(
                        "{crc_errors} chunk checksum(s) fail, first at {first_bad_chunk}; \
                         the file is complete but partially overwritten"
                    ),
                )
                .with("width", dims.0)
                .with("height", dims.1)
                .with("chunks", chunks)
                .with("crc_errors", crc_errors)
                // Where the damage begins. The length stays the whole file,
                // which the chunk structure still establishes.
                .with("damage_at", first_bad_at)
            } else {
                out
            };
        }
    }
}

fn truncated(at: usize, saw_idat: bool, why: &str) -> Outcome {
    if saw_idat {
        Outcome::partial(at as u64, format!("truncated: {why}")).truncated()
    } else {
        Outcome::reject(format!("{why}, before any image data")).truncated()
    }
}

fn structural(at: usize, saw_idat: bool, why: &str) -> Outcome {
    if saw_idat {
        Outcome::partial(at as u64, format!("chunk chain broke: {why}"))
    } else {
        Outcome::reject(format!("chunk chain broke before any image data: {why}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc32::crc32_parts;
    use crate::validate::Status;

    fn chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = (data.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(data);
        v.extend_from_slice(&crc32_parts(&[ty, data]).to_be_bytes());
        v
    }

    fn ihdr(w: u32, h: u32) -> Vec<u8> {
        let mut d = w.to_be_bytes().to_vec();
        d.extend_from_slice(&h.to_be_bytes());
        d.extend_from_slice(&[8, 2, 0, 0, 0]);
        chunk(b"IHDR", &d)
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = SIGNATURE.to_vec();
        v.extend(ihdr(w, h));
        v.extend(chunk(
            b"IDAT",
            &[0x78, 0x9C, 0x63, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01],
        ));
        v.extend(chunk(b"IEND", &[]));
        v
    }

    #[test]
    fn accepts_a_well_formed_png() {
        let p = png(16, 9);
        let out = validate(&p);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, p.len() as u64);
        assert_eq!(out.evidence_of("width"), Some("16"));
        assert_eq!(out.evidence_of("height"), Some("9"));
        assert_eq!(out.evidence_of("crc_errors"), Some("0"));
    }

    /// Trailing bytes must not extend the reported length: a carved PNG is
    /// followed by whatever was next on the disk.
    #[test]
    fn length_stops_at_iend_not_at_the_end_of_the_buffer() {
        let mut p = png(4, 4);
        let real = p.len();
        p.extend_from_slice(&[0xAB; 4096]);
        let out = validate(&p);
        assert_eq!(out.status, Status::Valid);
        assert_eq!(out.length, real as u64);
    }

    /// The case SPEC.md section 5.7 calls YELLOW: whole, but overwritten.
    #[test]
    fn a_corrupt_idat_is_partial_and_names_the_chunk() {
        let mut p = png(4, 4);
        let idat_payload = 8 + 25 + 8;
        p[idat_payload] ^= 0xFF;
        let out = validate(&p);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert_eq!(out.evidence_of("crc_errors"), Some("1"));
        assert!(out.detail.contains("IDAT"), "{}", out.detail);
    }

    #[test]
    fn a_corrupt_ihdr_is_rejected_rather_than_trusted() {
        let mut p = png(4, 4);
        p[16] ^= 0xFF; // inside IHDR's width
        assert_eq!(validate(&p).status, Status::Rejected);
    }

    #[test]
    fn rejects_a_bogus_first_chunk() {
        let mut v = SIGNATURE.to_vec();
        v.extend(chunk(b"IDAT", &[1, 2, 3]));
        v.extend(chunk(b"IEND", &[]));
        assert_eq!(validate(&v).status, Status::Rejected);
    }

    #[test]
    fn rejects_an_absurd_chunk_length() {
        let mut v = SIGNATURE.to_vec();
        v.extend(ihdr(4, 4));
        v.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        v.extend_from_slice(b"IDAT");
        assert_eq!(validate(&v).status, Status::Rejected);
    }

    #[test]
    fn a_truncated_png_after_idat_is_partial() {
        let p = png(64, 64);
        let out = validate(&p[..p.len() - 6]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
    }

    #[test]
    fn rejects_invalid_ihdr_fields() {
        for (depth, colour) in [(3u8, 2u8), (8, 7), (0, 0)] {
            let mut d = 4u32.to_be_bytes().to_vec();
            d.extend_from_slice(&4u32.to_be_bytes());
            d.extend_from_slice(&[depth, colour, 0, 0, 0]);
            let mut v = SIGNATURE.to_vec();
            v.extend(chunk(b"IHDR", &d));
            v.extend(chunk(b"IEND", &[]));
            assert_eq!(
                validate(&v).status,
                Status::Rejected,
                "depth {depth} colour {colour} should be rejected"
            );
        }
    }
}
