//! MP4/MOV/HEIF validator: bounded `ftyp`, then a box-tree walk.
//!
//! ISO base media files are a tree of boxes, each declaring its own size. That
//! makes the format easy to walk and easy to walk wrongly: a single bad size
//! field sends the walk into the middle of a payload, where the next four
//! bytes are read as another size, and the walk wanders until it runs out of
//! data. The result is a plausible-looking length that is nonsense.
//!
//! Two constraints keep that from happening.
//!
//! First, `ftyp` is bounded. It holds a four-character brand, a version and a
//! short list of compatible brands - tens of bytes. Nothing legitimate makes
//! it large, so a `ftyp` claiming megabytes means the four bytes before it
//! were not a size field, and the candidate is a chance match.
//!
//! Second, a real file has both `moov` and `mdat`. `moov` is the index and
//! `mdat` is the media; a file with only one of them cannot be played, and
//! saying so is more useful than emitting it as though it were fine.

use super::{be32, be64, Outcome};

/// A `ftyp` box is a brand, a minor version and a compatible-brands list.
/// Anything past this is not a file type box.
const MAX_FTYP: u64 = 1024;

/// Top-level boxes we expect to see. Unknown types are tolerated - the format
/// is extensible - but a walk that never sees a known one is not an MP4.
fn is_known_top_level(ty: &[u8; 4]) -> bool {
    matches!(
        ty,
        b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"pnot" | b"meta"
            | b"moof" | b"mfra" | b"uuid" | b"styp" | b"sidx" | b"pdin" | b"junk"
    )
}

pub fn validate(d: &[u8]) -> Outcome {
    if d.len() < 16 {
        return Outcome::reject("shorter than a file type box");
    }
    // The signature matches `ftyp` at offset 4, so the size precedes it.
    if &d[4..8] != b"ftyp" {
        return Outcome::reject("no ftyp box at offset 4");
    }
    let ftyp_size = be32(d, 0).unwrap_or(0) as u64;
    if ftyp_size < 8 {
        return Outcome::reject("the ftyp box declares a size below its own header");
    }
    if ftyp_size > MAX_FTYP {
        return Outcome::reject(format!(
            "the ftyp box declares {ftyp_size} bytes; a real one is tens of bytes, \
             so the preceding four bytes were not a size field"
        ));
    }
    let brand = String::from_utf8_lossy(&d[8..12]).to_string();
    if !d[8..12].iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        return Outcome::reject("the major brand is not four printable characters");
    }

    let mut at = 0u64;
    let mut saw_moov = false;
    let mut saw_mdat = false;
    let mut boxes = 0u64;
    let mut unknown = 0u64;
    let mut truncated_at: Option<u64> = None;

    while let Ok(i) = usize::try_from(at) {
        if i >= d.len() {
            break;
        }
        // A final partial box header means the file was cut mid-structure.
        if i + 8 > d.len() {
            truncated_at = Some(at);
            break;
        }
        let size32 = be32(d, i).unwrap_or(0) as u64;
        let Some(ty_slice) = d.get(i + 4..i + 8) else {
            truncated_at = Some(at);
            break;
        };
        let ty: [u8; 4] = [ty_slice[0], ty_slice[1], ty_slice[2], ty_slice[3]];
        // Box types are four printable characters. Hitting anything else means
        // the walk has left the structure.
        if !ty.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            break;
        }

        let (size, header) = match size32 {
            // 1 means the real size is a 64-bit value after the type.
            1 => match be64(d, i + 8) {
                Some(s) => (s, 16u64),
                None => {
                    truncated_at = Some(at);
                    break;
                }
            },
            // 0 means the box runs to the end of the file.
            0 => ((d.len() as u64).saturating_sub(at), 8u64),
            s => (s, 8u64),
        };

        if size < header {
            // A size smaller than its own header would make the walk loop
            // forever. This is the check that turns a corrupt field into a
            // bounded result instead of a hang.
            break;
        }

        match &ty {
            b"moov" => saw_moov = true,
            b"mdat" => saw_mdat = true,
            _ => {}
        }
        if !is_known_top_level(&ty) {
            unknown += 1;
        }
        boxes += 1;

        let Some(next) = at.checked_add(size) else { break };
        if next > d.len() as u64 {
            // The last box claims more than we have.
            truncated_at = Some(at);
            break;
        }
        at = next;
    }

    let evidence = |o: Outcome| {
        o.with("major_brand", brand.clone())
            .with("top_level_boxes", boxes)
            .with("unknown_boxes", unknown)
            .with("has_moov", saw_moov)
            .with("has_mdat", saw_mdat)
    };

    if boxes < 2 {
        return Outcome::reject("only the ftyp box could be walked; not a media file");
    }

    if let Some(cut) = truncated_at {
        let why = if saw_moov && !saw_mdat {
            "truncated: the index is present but the media data is cut short"
        } else if saw_mdat && !saw_moov {
            // SPEC.md section 5.7 calls a missing container index RED, and
            // section 5.6 item 4 is the reassembly path that needs it.
            "truncated: media data without a moov index; sample offsets are unknown"
        } else {
            "truncated: the box tree runs past the available data"
        };
        return evidence(Outcome::partial(cut.max(1), why).with("truncated", true));
    }

    if !saw_moov || !saw_mdat {
        let missing = if saw_moov { "mdat" } else { "moov" };
        return evidence(Outcome::partial(
            at,
            format!("the box tree is complete but there is no {missing} box"),
        ));
    }

    evidence(Outcome::valid(at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    fn boxed(ty: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(payload);
        v
    }

    fn ftyp() -> Vec<u8> {
        boxed(b"ftyp", b"isom\x00\x00\x02\x00isomiso2avc1mp41")
    }

    fn mp4() -> Vec<u8> {
        let mut v = ftyp();
        v.extend(boxed(b"moov", &[0x11; 64]));
        v.extend(boxed(b"mdat", &[0x22; 256]));
        v
    }

    #[test]
    fn accepts_a_well_formed_mp4() {
        let m = mp4();
        let out = validate(&m);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, m.len() as u64);
        assert_eq!(out.evidence_of("major_brand"), Some("isom"));
        assert_eq!(out.evidence_of("has_moov"), Some("true"));
        assert_eq!(out.evidence_of("has_mdat"), Some("true"));
    }

    #[test]
    fn trailing_disk_content_does_not_extend_the_length() {
        let mut m = mp4();
        let real = m.len();
        m.extend_from_slice(&[0x00; 4096]);
        assert_eq!(validate(&m).length, real as u64);
    }

    /// The bound that turns a chance `ftyp` into a rejection.
    #[test]
    fn rejects_an_ftyp_claiming_an_absurd_size() {
        let mut v = 0x4000_0000u32.to_be_bytes().to_vec();
        v.extend_from_slice(b"ftypisom");
        v.extend_from_slice(&[0u8; 32]);
        let out = validate(&v);
        assert_eq!(out.status, Status::Rejected, "{}", out.detail);
        assert!(out.detail.contains("tens of bytes"), "{}", out.detail);
    }

    #[test]
    fn handles_a_64_bit_box_size() {
        let mut v = ftyp();
        v.extend(boxed(b"moov", &[0x11; 32]));
        // mdat with size==1 and a 64-bit largesize.
        let payload = [0x33u8; 128];
        let total = 16 + payload.len() as u64;
        v.extend_from_slice(&1u32.to_be_bytes());
        v.extend_from_slice(b"mdat");
        v.extend_from_slice(&total.to_be_bytes());
        v.extend_from_slice(&payload);
        let out = validate(&v);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, v.len() as u64);
    }

    /// `size == 0` means "to the end of the file", which is legal for the last
    /// box and is how some muxers write a streaming mdat.
    #[test]
    fn a_zero_size_final_box_runs_to_the_end() {
        let mut v = ftyp();
        v.extend(boxed(b"moov", &[0x11; 32]));
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(b"mdat");
        v.extend_from_slice(&[0x44; 100]);
        let out = validate(&v);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, v.len() as u64);
    }

    /// A size field smaller than the box header would make a naive walk loop
    /// forever rather than fail.
    #[test]
    fn a_box_size_below_its_own_header_terminates_the_walk() {
        let mut v = ftyp();
        v.extend(boxed(b"moov", &[0x11; 16]));
        v.extend_from_slice(&4u32.to_be_bytes());
        v.extend_from_slice(b"mdat");
        v.extend_from_slice(&[0x55; 64]);
        let out = validate(&v);
        // Must terminate and must not claim the bytes after the bad box.
        assert!(out.length < v.len() as u64);
        assert_ne!(out.status, Status::Valid);
    }

    #[test]
    fn media_without_an_index_is_partial_and_says_so() {
        let mut v = ftyp();
        v.extend(boxed(b"mdat", &[0x22; 128]));
        let out = validate(&v);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(out.detail.contains("moov"), "{}", out.detail);
    }

    #[test]
    fn a_truncated_mp4_is_partial() {
        let m = mp4();
        let out = validate(&m[..m.len() - 100]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(out.length > 0);
    }

    #[test]
    fn rejects_ftyp_alone() {
        assert_eq!(validate(&ftyp()).status, Status::Rejected);
    }
}
