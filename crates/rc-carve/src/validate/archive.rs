//! Validators for archives that state their own size: 7z, CAB, RAR.
//!
//! These three are grouped because they share a shape. Each declares its total
//! length, or enough of one to compute it, within the first few dozen bytes -
//! so unlike a footerless format they need no search and no heuristic, and
//! unlike a compressed stream they need no decoding. Reading the field back and
//! checking it is consistent is the whole job.
//!
//! That is worth having: before these, every 7z, CAB and RAR on a carved volume
//! came back with a start offset and no end.

use super::{le16, le32, le64, Outcome};
use crate::crc32::crc32;

// ---------------------------------------------------------------------------
// 7z
// ---------------------------------------------------------------------------

const SEVENZ_MAGIC: &[u8; 6] = b"7z\xBC\xAF\x27\x1C";
/// Signature, version, the start header's CRC, and the 20-byte start header.
const SEVENZ_HEADER: u64 = 32;

/// 7z: the start header names where the metadata block sits, and the file ends
/// where that block ends.
///
/// The start header carries its own CRC-32, which is unusual and useful: it
/// makes a chance six-byte signature match essentially impossible to sustain.
pub fn validate_7z(d: &[u8]) -> Outcome {
    if d.len() < SEVENZ_HEADER as usize {
        return Outcome::reject("shorter than a 7z start header").truncated();
    }
    if &d[..6] != SEVENZ_MAGIC {
        return Outcome::reject("signature mismatch");
    }
    let (major, minor) = (d[6], d[7]);
    if major != 0 {
        return Outcome::reject(format!("format version {major}.{minor} is not 0.x"));
    }

    let want_crc = le32(d, 8).unwrap_or(0);
    let start_header = &d[12..32];
    let got_crc = crc32(start_header);
    let next_offset = le64(d, 12).unwrap_or(0);
    let next_size = le64(d, 20).unwrap_or(0);

    if want_crc != got_crc {
        // The start header is 20 bytes covered by a CRC stored beside it. If
        // that fails, the offsets read out of it cannot be trusted at all.
        return Outcome::reject(
            "the start header's CRC-32 does not match; the offsets in it are not usable",
        );
    }

    let Some(total) = SEVENZ_HEADER
        .checked_add(next_offset)
        .and_then(|v| v.checked_add(next_size))
    else {
        return Outcome::reject("the next-header offset and size overflow");
    };

    let out = |o: Outcome| {
        o.with("version", format!("{major}.{minor}"))
            .with("next_header_offset", next_offset)
            .with("next_header_size", next_size)
    };

    if total > d.len() as u64 {
        // The size is established even though the data is not all here: it came
        // from a CRC-checked header, not from how much we happened to read.
        return out(Outcome::partial(
            total,
            format!(
                "the start header declares {total} bytes but only {} are available",
                d.len()
            ),
        )
        .established()
        .truncated());
    }
    out(Outcome::valid(total))
}

// ---------------------------------------------------------------------------
// CAB
// ---------------------------------------------------------------------------

/// Microsoft cabinet: `cbCabinet` at offset 8 is the total size, in the header.
pub fn validate_cab(d: &[u8]) -> Outcome {
    if d.len() < 36 {
        return Outcome::reject("shorter than a CFHEADER").truncated();
    }
    if &d[..4] != b"MSCF" {
        return Outcome::reject("signature mismatch");
    }
    // Three reserved DWORDs that every writer leaves zero. Checking them is
    // most of what keeps a chance "MSCF" out.
    if le32(d, 4) != Some(0) || le32(d, 12) != Some(0) || le32(d, 20) != Some(0) {
        return Outcome::reject("a reserved field in the CFHEADER is non-zero");
    }

    let total = le32(d, 8).unwrap_or(0) as u64;
    let first_file = le32(d, 16).unwrap_or(0) as u64;
    let minor = d[24];
    let major = d[25];
    let folders = le16(d, 26).unwrap_or(0);
    let files = le16(d, 28).unwrap_or(0);

    if (major, minor) != (1, 3) {
        return Outcome::reject(format!("cabinet version {major}.{minor} is not 1.3"));
    }
    if total < 36 {
        return Outcome::reject("the declared cabinet size is smaller than its own header");
    }
    if first_file < 36 || first_file > total {
        return Outcome::reject("the first-file offset lies outside the cabinet");
    }
    if folders == 0 || files == 0 {
        return Outcome::reject("the cabinet declares no folders or no files");
    }

    let out = |o: Outcome| {
        o.with("version", format!("{major}.{minor}"))
            .with("folders", folders)
            .with("files", files)
    };
    if total > d.len() as u64 {
        return out(Outcome::partial(
            total,
            format!(
                "the header declares {total} bytes but only {} are available",
                d.len()
            ),
        )
        .established()
        .truncated());
    }
    out(Outcome::valid(total))
}

// ---------------------------------------------------------------------------
// RAR
// ---------------------------------------------------------------------------

/// RAR 4 and RAR 5 share a marker but nothing else, so this only confirms the
/// marker and the block that must follow it.
///
/// Neither version states a total size in a fixed place - the length comes from
/// walking a block chain whose encoding differs between the two - so this
/// establishes the format and not the end. It is still worth having: it turns
/// a seven-byte signature into a checked one.
pub fn validate_rar(d: &[u8]) -> Outcome {
    const RAR4: &[u8] = b"Rar!\x1A\x07\x00";
    const RAR5: &[u8] = b"Rar!\x1A\x07\x01\x00";

    if d.starts_with(RAR5) {
        // RAR5's first block after the marker is a vint-encoded CRC and size.
        if d.len() < RAR5.len() + 8 {
            return Outcome::reject("truncated immediately after the RAR5 marker");
        }
        return Outcome::partial(
            RAR5.len() as u64,
            "RAR5 marker and a following block; the archive's end needs a block walk, \
             so this length covers the marker only",
        )
        .with("version", 5);
    }
    if d.starts_with(RAR4) {
        if d.len() < RAR4.len() + 7 {
            return Outcome::reject("truncated immediately after the RAR4 marker");
        }
        return Outcome::partial(
            RAR4.len() as u64,
            "RAR4 marker and a following block; the archive's end needs a block walk, \
             so this length covers the marker only",
        )
        .with("version", 4);
    }
    Outcome::reject("not a RAR marker")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    fn sevenz(payload: usize) -> Vec<u8> {
        let next_offset = payload as u64;
        let next_size = 64u64;
        let mut start = next_offset.to_le_bytes().to_vec();
        start.extend_from_slice(&next_size.to_le_bytes());
        start.extend_from_slice(&0u32.to_le_bytes()); // next header CRC
        assert_eq!(start.len(), 20);

        let mut v = SEVENZ_MAGIC.to_vec();
        v.extend_from_slice(&[0, 4]);
        v.extend_from_slice(&crc32(&start).to_le_bytes());
        v.extend_from_slice(&start);
        v.resize(32 + payload + next_size as usize, 0x5A);
        v
    }

    fn cab(total: u32) -> Vec<u8> {
        let mut v = b"MSCF".to_vec();
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&total.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&36u32.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(&[3, 1]);
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.resize(total as usize, 0xC4);
        v
    }

    #[test]
    fn seven_zip_length_comes_from_the_start_header() {
        let a = sevenz(4000);
        let out = validate_7z(&a);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, a.len() as u64);
    }

    #[test]
    fn seven_zip_rejects_a_corrupt_start_header() {
        let mut a = sevenz(1000);
        a[13] ^= 0xFF; // inside the CRC-covered start header
        let out = validate_7z(&a);
        assert_eq!(out.status, Status::Rejected, "{}", out.detail);
        assert!(out.detail.contains("CRC"), "{}", out.detail);
    }

    #[test]
    fn seven_zip_trailing_bytes_do_not_extend_the_length() {
        let mut a = sevenz(2000);
        let real = a.len();
        a.extend_from_slice(&[0; 8192]);
        assert_eq!(validate_7z(&a).length, real as u64);
    }

    #[test]
    fn cab_length_is_the_declared_cabinet_size() {
        let c = cab(9000);
        let out = validate_cab(&c);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, 9000);
        assert_eq!(out.evidence_of("version"), Some("1.3"));
    }

    #[test]
    fn cab_rejects_non_zero_reserved_fields() {
        let mut c = cab(9000);
        c[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(validate_cab(&c).status, Status::Rejected);
    }

    #[test]
    fn cab_rejects_a_first_file_offset_outside_the_cabinet() {
        let mut c = cab(9000);
        c[16..20].copy_from_slice(&99_999u32.to_le_bytes());
        assert_eq!(validate_cab(&c).status, Status::Rejected);
    }

    #[test]
    fn a_truncated_cab_keeps_its_declared_size() {
        let c = cab(9000);
        let out = validate_cab(&c[..4000]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert_eq!(out.length, 9000, "the header size is still trustworthy");
        assert!(out.length_established);
    }

    #[test]
    fn rar_versions_are_told_apart() {
        let mut four = b"Rar!\x1A\x07\x00".to_vec();
        four.extend_from_slice(&[0x11; 64]);
        assert_eq!(validate_rar(&four).evidence_of("version"), Some("4"));

        let mut five = b"Rar!\x1A\x07\x01\x00".to_vec();
        five.extend_from_slice(&[0x11; 64]);
        assert_eq!(validate_rar(&five).evidence_of("version"), Some("5"));
    }

    /// RAR's length is not established, so it must never suppress a neighbour.
    #[test]
    fn rar_does_not_claim_an_established_length() {
        let mut v = b"Rar!\x1A\x07\x00".to_vec();
        v.extend_from_slice(&[0x11; 64]);
        let out = validate_rar(&v);
        assert_eq!(out.status, Status::Partial);
        assert!(!out.length_established);
    }

    #[test]
    fn rejects_chance_signatures() {
        let mut state = 0x1357_9BDFu32;
        for _ in 0..500 {
            let mut v = SEVENZ_MAGIC.to_vec();
            for _ in 0..64 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                v.push(state as u8);
            }
            assert_eq!(validate_7z(&v).status, Status::Rejected);

            let mut v = b"MSCF".to_vec();
            for _ in 0..64 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                v.push(state as u8);
            }
            assert_eq!(validate_cab(&v).status, Status::Rejected);
        }
    }
}
