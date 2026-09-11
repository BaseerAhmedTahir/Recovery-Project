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
    // Body start and end of the moov box, for the sample-level check below.
    let mut moov: Option<(usize, usize)> = None;
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
            b"moov" => {
                saw_moov = true;
                let body = i + header as usize;
                let end = (at + size).min(d.len() as u64) as usize;
                if body <= end {
                    moov = Some((body, end));
                }
            }
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

    // A box tree whose headers add up is not yet a file whose media is intact.
    //
    // The fragmented fixture's MP4 has all three top-level box headers inside
    // its first 4 KiB fragment, so the walk above read ftyp, moov and the mdat
    // header, summed their declared sizes to exactly the right 492,610 bytes,
    // and declared the file Valid without reading one byte of the media spread
    // across the other fifteen fragments. For H.264, the media itself can be
    // checked: see check_avc_samples.
    if let Some((body, end)) = moov {
        match check_avc_samples(d, body, end) {
            AvcCheck::Spliced {
                track,
                sample,
                offset,
            } => {
                return evidence(
                    Outcome::partial(
                        offset,
                        format!(
                            "the box tree adds up, but sample {sample} of H.264 track {track} \
                             does not divide into NAL units; the media data is spliced into \
                             something else - a fragment boundary or an overwrite - at that \
                             sample"
                        ),
                    )
                    .with("spliced_after", offset)
                    .with("bad_sample", sample),
                );
            }
            AvcCheck::Checked { samples } => {
                return evidence(Outcome::valid(at).with("avc_samples_checked", samples));
            }
            AvcCheck::NotApplicable => {}
        }
    }

    evidence(Outcome::valid(at))
}

// ---------------------------------------------------------------------------
// H.264 sample check
// ---------------------------------------------------------------------------

enum AvcCheck {
    /// No track is H.264 with an avcC configuration, so there is nothing a
    /// sample can be checked against.
    NotApplicable,
    /// Every H.264 sample within reach divided cleanly into NAL units.
    Checked { samples: u64 },
    /// This sample did not; `offset` is where it starts.
    Spliced { track: u64, sample: u64, offset: u64 },
}

/// Most samples to walk. The check is linear in the sample count, and a corrupt
/// count must not turn validation into an unbounded loop.
const MAX_SAMPLES: u64 = 2_000_000;

/// Check that every H.264 sample divides exactly into length-prefixed NAL units.
///
/// An H.264 sample in an MP4 is a run of NAL units, each preceded by its length
/// in a field whose width the track's `avcC` box declares. The lengths must tile
/// the sample exactly, and every NAL header's top bit - `forbidden_zero_bit` -
/// must be clear. Real encoder output always satisfies this; ffmpeg's H.264
/// files tile with no failures. Data from anywhere else almost never does: a
/// random length field falls inside a sample of a few kilobytes with odds of
/// about one in a million.
///
/// Applied only to `avc1`/`avc3` tracks that carry `avcC`, which is not a
/// convenience but a requirement: `avcC` is where the length-field width is
/// declared, so without it the samples cannot be parsed at all. That also keeps
/// the check off audio tracks, whose AAC samples are not NAL units, and off
/// files that claim `avc1` without the configuration a real encoder writes -
/// which is what this project's own corpus generator produces.
fn check_avc_samples(d: &[u8], moov_body: usize, moov_end: usize) -> AvcCheck {
    let mut any = false;
    let mut samples_checked = 0u64;

    for (idx, (trak_body, trak_end)) in children(d, moov_body, moov_end, b"trak")
        .into_iter()
        .enumerate()
    {
        let track_no = idx as u64 + 1;
        let Some(stbl) = descend(d, trak_body, trak_end, &[b"mdia", b"minf", b"stbl"]) else {
            continue;
        };
        let (stbl_body, stbl_end) = stbl;
        let Some(length_size) = avc_length_size(d, stbl_body, stbl_end) else {
            continue;
        };
        let (Some(sizes), Some(chunks), Some(stsc)) = (
            sample_sizes(d, stbl_body, stbl_end),
            chunk_offsets(d, stbl_body, stbl_end),
            samples_per_chunk(d, stbl_body, stbl_end),
        ) else {
            continue;
        };
        any = true;

        let mut sample = 0usize;
        let mut entry = 0usize;
        for (k, &chunk) in chunks.iter().enumerate() {
            // stsc lists runs: each entry applies from its first chunk until
            // the next entry's first chunk. Chunks are numbered from one.
            while entry + 1 < stsc.len() && (stsc[entry + 1].0 as usize) <= k + 1 {
                entry += 1;
            }
            let per = stsc.get(entry).map(|e| e.1).unwrap_or(1);
            let mut pos = chunk;
            for _ in 0..per {
                let Some(&size) = sizes.get(sample) else { break };
                let Some(end) = pos.checked_add(size as u64) else {
                    return AvcCheck::Spliced {
                        track: track_no,
                        sample: sample as u64,
                        offset: pos,
                    };
                };
                // Past the end of what we were given is truncation, which the
                // box walk has already reported, not evidence of a splice.
                if end > d.len() as u64 {
                    return if any {
                        AvcCheck::Checked {
                            samples: samples_checked,
                        }
                    } else {
                        AvcCheck::NotApplicable
                    };
                }
                if !tiles(&d[pos as usize..end as usize], length_size) {
                    return AvcCheck::Spliced {
                        track: track_no,
                        sample: sample as u64,
                        offset: pos,
                    };
                }
                samples_checked += 1;
                if samples_checked >= MAX_SAMPLES {
                    return AvcCheck::Checked {
                        samples: samples_checked,
                    };
                }
                pos = end;
                sample += 1;
            }
        }
    }
    if any {
        AvcCheck::Checked {
            samples: samples_checked,
        }
    } else {
        AvcCheck::NotApplicable
    }
}

/// Does `sample` divide exactly into length-prefixed NAL units?
fn tiles(sample: &[u8], length_size: usize) -> bool {
    let mut p = 0usize;
    while p < sample.len() {
        let Some(field) = sample.get(p..p + length_size) else {
            return false;
        };
        let len = field.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize);
        if len == 0 {
            return false;
        }
        let Some(nal) = sample.get(p + length_size) else {
            return false;
        };
        // forbidden_zero_bit: set in no conforming NAL unit.
        if nal & 0x80 != 0 {
            return false;
        }
        let Some(next) = p.checked_add(length_size + len) else {
            return false;
        };
        if next > sample.len() {
            return false;
        }
        p = next;
    }
    true
}

/// Child boxes of a given type within [start, end).
fn children(d: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = start;
    while i + 8 <= end {
        let size32 = be32(d, i).unwrap_or(0) as u64;
        let (size, hdr) = match size32 {
            1 => match be64(d, i + 8) {
                Some(s) => (s, 16usize),
                None => break,
            },
            0 => ((end - i) as u64, 8usize),
            s => (s, 8usize),
        };
        if size < hdr as u64 {
            break;
        }
        let Some(stop) = (i as u64).checked_add(size) else { break };
        let stop = stop.min(end as u64) as usize;
        if d.get(i + 4..i + 8) == Some(&want[..]) {
            out.push((i + hdr, stop));
        }
        if stop <= i {
            break;
        }
        i = stop;
    }
    out
}

/// Follow a path of box types down from [start, end).
fn descend(d: &[u8], start: usize, end: usize, path: &[&[u8; 4]]) -> Option<(usize, usize)> {
    let mut range = (start, end);
    for want in path {
        range = *children(d, range.0, range.1, want).first()?;
    }
    Some(range)
}

/// The NAL length-field width from the track's avcC, if the track is H.264.
fn avc_length_size(d: &[u8], stbl_body: usize, stbl_end: usize) -> Option<usize> {
    let (stsd_body, stsd_end) = *children(d, stbl_body, stbl_end, b"stsd").first()?;
    // stsd is a full box: version and flags, then an entry count.
    let entry = stsd_body + 8;
    let entry_type = d.get(entry + 4..entry + 8)?;
    if entry_type != b"avc1" && entry_type != b"avc3" {
        return None;
    }
    let entry_size = be32(d, entry)? as usize;
    let entry_end = (entry + entry_size).min(stsd_end);
    let window = d.get(entry..entry_end)?;
    let at = memchr::memmem::find(window, b"avcC")?;
    // 'avcC' tag, then the body: version, profile, compatibility, level, and
    // then the byte whose low two bits are lengthSizeMinusOne.
    let byte = *window.get(at + 8)?;
    Some((byte & 0x03) as usize + 1)
}

fn full_box_body(d: &[u8], stbl_body: usize, stbl_end: usize, want: &[u8; 4]) -> Option<(usize, usize)> {
    let (body, end) = *children(d, stbl_body, stbl_end, want).first()?;
    // Skip version and flags.
    Some((body + 4, end))
}

fn sample_sizes(d: &[u8], stbl_body: usize, stbl_end: usize) -> Option<Vec<u32>> {
    let (b, end) = full_box_body(d, stbl_body, stbl_end, b"stsz")?;
    let fixed = be32(d, b)?;
    let count = (be32(d, b + 4)? as u64).min(MAX_SAMPLES) as usize;
    if fixed != 0 {
        return Some(vec![fixed; count]);
    }
    let mut v = Vec::with_capacity(count);
    for k in 0..count {
        let at = b + 8 + 4 * k;
        if at + 4 > end {
            break;
        }
        v.push(be32(d, at)?);
    }
    Some(v)
}

fn chunk_offsets(d: &[u8], stbl_body: usize, stbl_end: usize) -> Option<Vec<u64>> {
    if let Some((b, end)) = full_box_body(d, stbl_body, stbl_end, b"stco") {
        let n = (be32(d, b)? as u64).min(MAX_SAMPLES) as usize;
        return Some(
            (0..n)
                .map(|k| b + 4 + 4 * k)
                .take_while(|at| at + 4 <= end)
                .filter_map(|at| be32(d, at).map(|v| v as u64))
                .collect(),
        );
    }
    let (b, end) = full_box_body(d, stbl_body, stbl_end, b"co64")?;
    let n = (be32(d, b)? as u64).min(MAX_SAMPLES) as usize;
    Some(
        (0..n)
            .map(|k| b + 4 + 8 * k)
            .take_while(|at| at + 8 <= end)
            .filter_map(|at| be64(d, at))
            .collect(),
    )
}

/// stsc entries as (first_chunk, samples_per_chunk).
fn samples_per_chunk(d: &[u8], stbl_body: usize, stbl_end: usize) -> Option<Vec<(u32, u32)>> {
    let (b, end) = full_box_body(d, stbl_body, stbl_end, b"stsc")?;
    let n = (be32(d, b)? as u64).min(MAX_SAMPLES) as usize;
    let mut v = Vec::with_capacity(n);
    for k in 0..n {
        let at = b + 4 + 12 * k;
        if at + 12 > end {
            break;
        }
        v.push((be32(d, at)?, be32(d, at + 4)?));
    }
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
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

    /// A minimal but correctly structured H.264 MP4: ftyp, mdat, moov, with an
    /// avc1 sample entry carrying avcC and samples that tile as 4-byte
    /// length-prefixed NAL units. mdat precedes moov so the chunk offsets are
    /// known before the index is written.
    fn avc_mp4(samples: usize, nals_per_sample: usize, nal_len: usize) -> (Vec<u8>, Vec<u64>) {
        fn b(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(ty);
            v.extend_from_slice(body);
            v
        }
        fn full(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
            let mut v = vec![0u8; 4];
            v.extend_from_slice(body);
            b(ty, &v)
        }

        let ftyp = b(b"ftyp", b"isom\x00\x00\x02\x00isomavc1");
        let mdat_data_at = (ftyp.len() + 8) as u64;

        let mut data = Vec::new();
        let mut offsets = Vec::new();
        let mut sizes = Vec::new();
        for s in 0..samples {
            offsets.push(mdat_data_at + data.len() as u64);
            let start = data.len();
            for n in 0..nals_per_sample {
                data.extend_from_slice(&(nal_len as u32).to_be_bytes());
                data.push(0x65); // nal_ref_idc 3, type 5: forbidden bit clear
                data.extend((1..nal_len).map(|i| ((i * 13 + s + n) % 251) as u8));
            }
            sizes.push((data.len() - start) as u32);
        }
        let mdat = b(b"mdat", &data);

        let mut avcc = vec![1u8, 0x42, 0x00, 0x1E, 0xFF, 0xE0, 0x00];
        avcc.truncate(7);
        let mut entry = vec![0u8; 6];
        entry.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
        entry.extend_from_slice(&[0u8; 70]); // visual sample entry fields
        entry.extend(b(b"avcC", &avcc));
        let mut stsd_body = 1u32.to_be_bytes().to_vec();
        stsd_body.extend(b(b"avc1", &entry));
        let stsd = full(b"stsd", &stsd_body);

        let mut stsz_body = 0u32.to_be_bytes().to_vec();
        stsz_body.extend_from_slice(&(samples as u32).to_be_bytes());
        for sz in &sizes {
            stsz_body.extend_from_slice(&sz.to_be_bytes());
        }
        let stsz = full(b"stsz", &stsz_body);

        let mut stsc_body = 1u32.to_be_bytes().to_vec();
        stsc_body.extend_from_slice(&1u32.to_be_bytes());
        stsc_body.extend_from_slice(&1u32.to_be_bytes());
        stsc_body.extend_from_slice(&1u32.to_be_bytes());
        let stsc = full(b"stsc", &stsc_body);

        let mut stco_body = (samples as u32).to_be_bytes().to_vec();
        for o in &offsets {
            stco_body.extend_from_slice(&(*o as u32).to_be_bytes());
        }
        let stco = full(b"stco", &stco_body);

        let mut stbl = stsd;
        stbl.extend(stsz);
        stbl.extend(stsc);
        stbl.extend(stco);
        let moov = b(
            b"moov",
            &b(b"trak", &b(b"mdia", &b(b"minf", &b(b"stbl", &stbl)))),
        );

        let mut v = ftyp;
        v.extend(mdat);
        v.extend(moov);
        (v, offsets)
    }

    /// Real H.264 media divides into NAL units, and the validator checks it.
    #[test]
    fn well_formed_h264_samples_are_checked_and_accepted() {
        let (m, _) = avc_mp4(12, 3, 200);
        let out = validate(&m);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, m.len() as u64);
        assert_eq!(out.evidence_of("avc_samples_checked"), Some("12"));
    }

    /// The case the fragmented fixture exposed. The box headers all sit in the
    /// first fragment and add up to the right length, so without looking at
    /// the samples the file validated as complete. A sample that does not
    /// divide into NAL units is where the media stops being this file.
    #[test]
    fn a_sample_that_does_not_tile_is_reported_as_a_splice() {
        let (mut m, offsets) = avc_mp4(12, 3, 200);
        // Overwrite sample 7 with bytes from somewhere else entirely.
        let at = offsets[7] as usize;
        for (i, byte) in m[at..at + 600].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(97).wrapping_add(0xA3);
        }
        let out = validate(&m);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(!out.length_established, "a splice point is a floor, not an end");
        assert_eq!(out.evidence_of("bad_sample"), Some("7"));
        assert_eq!(out.length, offsets[7], "should stop where the bad sample begins");
    }

    /// A NAL header with its forbidden bit set is not H.264, whatever the
    /// length fields say.
    #[test]
    fn a_set_forbidden_bit_fails_the_sample() {
        let (mut m, offsets) = avc_mp4(4, 2, 100);
        m[offsets[2] as usize + 4] |= 0x80;
        let out = validate(&m);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert_eq!(out.evidence_of("bad_sample"), Some("2"));
    }

    /// No avcC, no check - there is no length-field width to parse with. This
    /// is what keeps the corpus generator's avc1-without-avcC files, and every
    /// non-H.264 track, exactly as they were.
    #[test]
    fn without_avcc_the_samples_are_not_checked() {
        let m = mp4();
        let out = validate(&m);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.evidence_of("avc_samples_checked"), None);
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
