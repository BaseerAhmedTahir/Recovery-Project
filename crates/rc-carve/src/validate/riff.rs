//! RIFF validator, shared by WAV, AVI and WebP.
//!
//! `RIFF` is four bytes and identifies a container, not a format. WAV, AVI and
//! WebP all start with it, and `signatures.json` has a separate entry for each
//! because the extension the user wants is the inner one. The form type at
//! offset 8 is what tells them apart, so each of the three public entry points
//! here is the same walk with a different required form and different required
//! chunks.
//!
//! The structural check is that the chunk sizes tile the declared RIFF size
//! exactly. Each chunk declares its own length and is padded to an even
//! boundary - the pad byte is not counted in the size field, which is the
//! detail most naive parsers get wrong and which makes their offsets drift by
//! one for the rest of the file.
//!
//! **Untested against real files.** The corpus has no WAV, AVI or WebP, so
//! these are exercised only against hand-built byte vectors. See
//! `docs/LIMITATIONS.md` section 4.

use super::{be32, le32, Outcome};

pub fn validate_wav(d: &[u8]) -> Outcome {
    validate_form(d, b"WAVE", &[b"fmt ", b"data"], "wav")
}

pub fn validate_avi(d: &[u8]) -> Outcome {
    // AVI keeps its stream headers and its frames in LIST chunks, so the
    // required names are list types rather than chunk ids.
    validate_form(d, b"AVI ", &[b"hdrl", b"movi"], "avi")
}

pub fn validate_webp(d: &[u8]) -> Outcome {
    // Any one of the three bitstream chunks is enough: VP8 is lossy, VP8L is
    // lossless, VP8X is the extended form used for animation and alpha.
    validate_form(d, b"WEBP", &[], "webp")
}

/// AIFF: the same idea as RIFF with the bytes the other way round.
///
/// `FORM` is Electronic Arts' IFF container, which RIFF is a little-endian
/// copy of, so the walk is identical apart from the byte order - which is
/// exactly the kind of detail that gets copied wrong. Keeping it beside the
/// RIFF walk rather than in its own file makes the difference visible.
pub fn validate_aiff(d: &[u8]) -> Outcome {
    if d.len() < 12 {
        return Outcome::reject("shorter than a FORM header").truncated();
    }
    if &d[..4] != b"FORM" {
        return Outcome::reject("does not begin with FORM");
    }
    let form_size = be32(d, 4).unwrap_or(0) as u64;
    if form_size < 4 {
        return Outcome::reject("the FORM size field is smaller than its own form type");
    }
    let form = &d[8..12];
    // AIFF is uncompressed; AIFC is the compressed variant and uses the same
    // chunk layout.
    if form != b"AIFF" && form != b"AIFC" {
        return Outcome::reject(format!(
            "form type is {:?}, not AIFF or AIFC",
            String::from_utf8_lossy(form)
        ));
    }
    let declared_total = form_size + 8;

    let mut at = 12usize;
    let mut seen: Vec<[u8; 4]> = Vec::new();
    let mut chunks = 0u64;
    let mut walked_past_end = false;
    let limit = (declared_total as usize).min(d.len());

    while at + 8 <= limit {
        let id: [u8; 4] = [d[at], d[at + 1], d[at + 2], d[at + 3]];
        if !id.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            break;
        }
        let size = be32(d, at + 4).unwrap_or(0) as u64;
        seen.push(id);
        chunks += 1;
        // Chunks pad to an even boundary and the pad byte is not counted,
        // exactly as in RIFF.
        let advance = size + (size & 1);
        let Some(next) = (at as u64 + 8).checked_add(advance) else {
            walked_past_end = true;
            break;
        };
        if next > d.len() as u64 {
            walked_past_end = true;
            break;
        }
        at = next as usize;
    }

    if chunks == 0 {
        return Outcome::reject("no chunks follow the FORM header");
    }
    let missing: Vec<&str> = [&b"COMM"[..], &b"SSND"[..]]
        .iter()
        .filter(|r| !seen.iter().any(|c| c[..] == ***r))
        .map(|r| std::str::from_utf8(r).unwrap_or("?"))
        .collect();

    let evidence = |o: Outcome| {
        o.with_ext("aiff")
            .with("form", String::from_utf8_lossy(form).to_string())
            .with("chunks", chunks)
            .with("declared_bytes", declared_total)
    };

    if !missing.is_empty() {
        return evidence(Outcome::partial(
            (at as u64).min(d.len() as u64),
            format!("required chunk(s) missing: {}", missing.join(", ")),
        ));
    }
    if declared_total > d.len() as u64 || walked_past_end {
        return evidence(
            Outcome::partial(
                (at as u64).min(d.len() as u64),
                format!(
                    "declares {declared_total} bytes but only {} are available; truncated",
                    d.len()
                ),
            )
            .truncated(),
        );
    }
    evidence(Outcome::valid(declared_total))
}

fn validate_form(
    d: &[u8],
    want_form: &[u8; 4],
    required: &[&[u8; 4]],
    ext: &'static str,
) -> Outcome {
    if d.len() < 12 {
        return Outcome::reject("shorter than a RIFF header").truncated();
    }
    if &d[..4] != b"RIFF" {
        return Outcome::reject("does not begin with RIFF");
    }
    let riff_size = le32(d, 4).unwrap_or(0) as u64;
    if riff_size < 4 {
        return Outcome::reject("the RIFF size field is smaller than its form type");
    }
    let form = &d[8..12];
    if form != want_form {
        return Outcome::reject(format!(
            "form type is {:?}, not {:?}",
            String::from_utf8_lossy(form),
            String::from_utf8_lossy(want_form)
        ));
    }

    // The size field counts everything after itself, so the file is 8 longer.
    let declared_total = riff_size + 8;

    let mut at = 12usize;
    let mut seen: Vec<[u8; 4]> = Vec::new();
    let mut chunks = 0u64;
    let mut walked_past_end = false;
    let limit = (declared_total as usize).min(d.len());

    while at + 8 <= limit {
        let id: [u8; 4] = [d[at], d[at + 1], d[at + 2], d[at + 3]];
        // Chunk ids are four printable characters.
        if !id.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            break;
        }
        let size = le32(d, at + 4).unwrap_or(0) as u64;
        let body = at + 8;

        if &id == b"LIST" || &id == b"RIFF" {
            // A list's first four payload bytes name what kind of list it is,
            // and its children follow. Recording the list type is what lets
            // the AVI check ask for `hdrl` and `movi`.
            if body + 4 <= d.len() {
                seen.push([d[body], d[body + 1], d[body + 2], d[body + 3]]);
            }
        } else {
            seen.push(id);
        }
        chunks += 1;

        // Chunks pad to an even boundary, and the pad byte is NOT counted in
        // the size field.
        let advance = size + (size & 1);
        let Some(next) = (body as u64).checked_add(advance) else {
            walked_past_end = true;
            break;
        };
        if next > d.len() as u64 {
            walked_past_end = true;
            break;
        }
        at = next as usize;
    }

    if chunks == 0 {
        let out = Outcome::reject("no chunks follow the RIFF header");
        // Either the first chunk id was not a chunk id - not this format - or
        // the data ended before a whole chunk header, while the RIFF size says
        // there is more. Only the second is a truncation.
        let cut_short = at + 8 > d.len() && (d.len() as u64) < declared_total;
        return if cut_short { out.truncated() } else { out };
    }

    let missing: Vec<String> = required
        .iter()
        .filter(|r| !seen.contains(**r))
        .map(|r| String::from_utf8_lossy(*r).trim_end().to_string())
        .collect();

    // WebP needs one of three alternatives rather than all of a set.
    let webp_ok = want_form != b"WEBP"
        || seen
            .iter()
            .any(|c| c == b"VP8 " || c == b"VP8L" || c == b"VP8X");

    let evidence = |o: Outcome| {
        o.with_ext(ext)
            .with("form", String::from_utf8_lossy(form).to_string())
            .with("chunks", chunks)
            .with("declared_bytes", declared_total)
    };

    if !webp_ok {
        return evidence(Outcome::partial(
            (at as u64).min(d.len() as u64),
            "a WEBP container with no VP8, VP8L or VP8X bitstream chunk",
        ));
    }
    if !missing.is_empty() {
        return evidence(Outcome::partial(
            (at as u64).min(d.len() as u64),
            format!("required chunk(s) missing: {}", missing.join(", ")),
        ));
    }

    if declared_total > d.len() as u64 || walked_past_end {
        // `at` is where the chunk walk stopped, which the data justifies.
        // `d.len()` would be the caller's buffer size.
        return evidence(
            Outcome::partial(
                (at as u64).min(d.len() as u64),
                format!(
                    "declares {declared_total} bytes but only {} are available; \
                     truncated, so this length runs to the last complete chunk",
                    d.len()
                ),
            )
            .truncated(),
        );
    }

    evidence(Outcome::valid(declared_total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&(data.len() as u32).to_le_bytes());
        v.extend_from_slice(data);
        if data.len() % 2 == 1 {
            v.push(0); // pad byte, not counted in the size field
        }
        v
    }

    fn riff(form: &[u8; 4], body: Vec<u8>) -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        v.extend_from_slice(form);
        v.extend_from_slice(&body);
        v
    }

    fn wav() -> Vec<u8> {
        let mut body = chunk(
            b"fmt ",
            &[1, 0, 2, 0, 0x44, 0xAC, 0, 0, 0x10, 0xB1, 2, 0, 4, 0, 16, 0],
        );
        body.extend(chunk(b"data", &[0x00; 128]));
        riff(b"WAVE", body)
    }

    fn avi() -> Vec<u8> {
        let mut hdrl = b"hdrl".to_vec();
        hdrl.extend(chunk(b"avih", &[0x11; 56]));
        let mut movi = b"movi".to_vec();
        movi.extend(chunk(b"00dc", &[0x22; 64]));
        let mut body = chunk(b"LIST", &hdrl);
        body.extend(chunk(b"LIST", &movi));
        riff(b"AVI ", body)
    }

    fn aiff() -> Vec<u8> {
        fn bchunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut v = id.to_vec();
            v.extend_from_slice(&(data.len() as u32).to_be_bytes());
            v.extend_from_slice(data);
            if data.len() % 2 == 1 {
                v.push(0);
            }
            v
        }
        let rate = [0x40u8, 0x0e, 0xac, 0x44, 0, 0, 0, 0, 0, 0];
        let frames = vec![0x33u8; 2048];
        let mut comm = (1u16).to_be_bytes().to_vec();
        comm.extend_from_slice(&((frames.len() / 2) as u32).to_be_bytes());
        comm.extend_from_slice(&16u16.to_be_bytes());
        comm.extend_from_slice(&rate);
        let mut ssnd = 0u32.to_be_bytes().to_vec();
        ssnd.extend_from_slice(&0u32.to_be_bytes());
        ssnd.extend_from_slice(&frames);

        let mut body = b"AIFF".to_vec();
        body.extend(bchunk(b"COMM", &comm));
        body.extend(bchunk(b"SSND", &ssnd));
        let mut v = b"FORM".to_vec();
        v.extend_from_slice(&(body.len() as u32).to_be_bytes());
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn accepts_a_well_formed_aiff() {
        let a = aiff();
        let out = validate_aiff(&a);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, a.len() as u64);
        assert_eq!(out.refined_ext, Some("aiff"));
    }

    /// The byte order is the whole difference from RIFF, so a WAV must not
    /// validate as an AIFF and vice versa.
    #[test]
    fn aiff_and_wav_do_not_accept_each_other() {
        assert_ne!(validate_aiff(&wav()).status, Status::Valid);
        assert_ne!(validate_wav(&aiff()).status, Status::Valid);
    }

    #[test]
    fn aiff_trailing_bytes_do_not_extend_the_length() {
        let mut a = aiff();
        let real = a.len();
        a.extend_from_slice(&[0xEE; 4096]);
        assert_eq!(validate_aiff(&a).length, real as u64);
    }

    #[test]
    fn accepts_a_well_formed_wav() {
        let w = wav();
        let out = validate_wav(&w);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, w.len() as u64);
        assert_eq!(out.refined_ext, Some("wav"));
    }

    #[test]
    fn accepts_a_well_formed_avi_by_its_list_types() {
        let a = avi();
        let out = validate_avi(&a);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, a.len() as u64);
    }

    #[test]
    fn accepts_a_webp_with_any_bitstream_chunk() {
        for id in [b"VP8 ", b"VP8L", b"VP8X"] {
            let w = riff(b"WEBP", chunk(id, &[0x33; 32]));
            let out = validate_webp(&w);
            assert_eq!(
                out.status,
                Status::Valid,
                "{:?}: {}",
                String::from_utf8_lossy(id),
                out.detail
            );
        }
    }

    /// The off-by-one that makes a naive parser drift for the rest of the
    /// file: an odd-length chunk is padded, but the pad is not in the size.
    #[test]
    fn an_odd_length_chunk_is_padded_without_the_pad_being_counted() {
        let mut body = chunk(b"fmt ", &[0x01; 15]); // odd
        body.extend(chunk(b"data", &[0x02; 33])); // odd
        let w = riff(b"WAVE", body);
        let out = validate_wav(&w);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, w.len() as u64);
        assert_eq!(out.evidence_of("chunks"), Some("2"));
    }

    #[test]
    fn trailing_disk_content_does_not_extend_the_length() {
        let mut w = wav();
        let real = w.len();
        w.extend_from_slice(&[0xEE; 4096]);
        assert_eq!(validate_wav(&w).length, real as u64);
    }

    /// The same four header bytes, three different formats. Asking the wrong
    /// validator must not produce a hit.
    #[test]
    fn the_three_forms_do_not_accept_each_other() {
        assert_eq!(validate_avi(&wav()).status, Status::Rejected);
        assert_eq!(validate_webp(&wav()).status, Status::Rejected);
        assert_eq!(validate_wav(&avi()).status, Status::Rejected);
    }

    #[test]
    fn a_wav_without_a_data_chunk_is_partial() {
        let w = riff(b"WAVE", chunk(b"fmt ", &[0x01; 16]));
        let out = validate_wav(&w);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(out.detail.contains("data"), "{}", out.detail);
    }

    #[test]
    fn a_truncated_wav_is_partial() {
        let w = wav();
        let out = validate_wav(&w[..w.len() - 40]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        // The length is what the chunk walk justified, not the buffer size.
        assert!(out.length <= (w.len() - 40) as u64);
    }

    #[test]
    fn rejects_a_chance_riff_in_noise() {
        let mut v = b"RIFF".to_vec();
        v.extend((0u8..120).map(|n| n.wrapping_mul(29).wrapping_add(7)));
        assert_eq!(validate_wav(&v).status, Status::Rejected);
    }

    #[test]
    fn a_size_field_claiming_more_than_exists_does_not_overrun() {
        let mut w = wav();
        w[4..8].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        let out = validate_wav(&w);
        assert!(out.length <= w.len() as u64);
        assert_ne!(out.status, Status::Valid);
    }
}
