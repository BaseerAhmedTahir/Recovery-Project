//! JPEG validator: marker walk, entropy scan, restart-marker sequencing.
//!
//! `FF D8 FF` is three bytes, so a scan of a 512 GiB drive of random data
//! would produce roughly 32000 spurious hits. It is also *deliberately*
//! present inside every JPEG carrying an EXIF thumbnail, which is most photos
//! any phone or camera ever wrote. A carver that trusts the header therefore
//! reports two or three candidates per real photo and thousands of candidates
//! that are not images at all.
//!
//! What separates a real JPEG from a chance `FF D8 FF` is that the marker
//! segments chain: each one declares its own length, and following those
//! lengths must land exactly on the next `FF`. A run of random bytes stops
//! chaining almost immediately.
//!
//! Note what this validator does *not* do: it does not Huffman-decode the
//! entropy-coded data. Decode-until-failure belongs to `rc-bifrag`
//! (SPEC.md section 5.6 item 5), where the point is to find a fragment
//! boundary rather than to accept or reject a candidate.

use super::{be16, Outcome};

// Markers that stand alone: no length field follows.
const TEM: u8 = 0x01;
const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const RST_FIRST: u8 = 0xD0;
const RST_LAST: u8 = 0xD7;

/// Define Restart Interval: declares how many MCUs lie between restart markers.
const DRI: u8 = 0xDD;

/// The most entropy-coded bytes one 8x8 block can occupy.
///
/// Derived from the format, not measured from a corpus. A baseline block holds
/// 64 coefficients; each is at most a 16-bit Huffman code plus an 11-bit
/// magnitude, about 216 bytes for the block, and byte stuffing (every 0xFF
/// written as FF 00) can at worst double that to 432. 512 leaves margin and
/// also covers 12-bit precision. Progressive scans carry a subset of each
/// block's coefficients, so they fall inside it too.
///
/// Real corpus JPEGs run 42 to 116 bytes between restart markers at an interval
/// of four one-block MCUs, far inside the 2048 this allows - so the bound cannot
/// fire on a legitimate file, and anything that exceeds it is not JPEG data.
const MAX_BYTES_PER_BLOCK: u64 = 512;

/// Start-of-frame markers. `C4` is DHT, `C8` is reserved and `CC` is DAC, so
/// they are holes in the range rather than frame headers.
fn is_sof(m: u8) -> bool {
    matches!(m, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF)
}

pub fn validate(d: &[u8]) -> Outcome {
    if d.len() < 4 {
        return Outcome::reject("shorter than a JPEG header");
    }
    if d[0] != 0xFF || d[1] != SOI {
        return Outcome::reject("does not begin with SOI");
    }

    let mut i = 2usize;
    let mut saw_sof = false;
    let mut saw_sos = false;
    let mut dims: Option<(u16, u16)> = None;
    let mut components = 0u8;
    let mut restarts = 0u64;
    // From DRI, in MCUs; zero means the file declares no restart markers.
    let mut restart_interval = 0u64;
    // Sum of each component's sampling factors, from the frame header.
    let mut blocks_per_mcu = 1u64;
    let mut segments = 0u64;
    // The furthest point the structure justifies: the end of the last segment
    // walked, or the last restart marker inside the scan. Reporting `d.len()`
    // instead would report the caller's buffer size, which for a carve is the
    // validation window and not a property of the file at all.
    let mut verified_to = 2usize;

    loop {
        // Fill bytes: a stream may pad with any number of FFs before a marker.
        let mut j = i;
        while j < d.len() && d[j] == 0xFF {
            j += 1;
        }
        if j >= d.len() {
            return truncated(
                verified_to,
                saw_sof,
                saw_sos,
                "ran out of data looking for a marker",
            );
        }
        // We must have crossed at least one FF to be at a marker at all.
        if j == i {
            return desync(i, saw_sof, saw_sos);
        }
        let marker = d[j];
        i = j + 1;

        match marker {
            EOI => {
                let len = i as u64;
                if !saw_sof || !saw_sos {
                    // A complete-looking stream with no frame and no scan is
                    // not an image. Random data reaches this often enough to
                    // matter, because FF D9 is only two bytes.
                    return Outcome::reject(
                        "reached EOI without a frame header and a scan; not an image",
                    );
                }
                let (w, h) = dims.unwrap_or((0, 0));
                // Reaching EOI means every restart marker was in sequence: an
                // out-of-sequence one ends the scan where it occurs.
                return Outcome::valid(len)
                    .with("width", w)
                    .with("height", h)
                    .with("components", components)
                    .with("segments", segments)
                    .with("restart_markers", restarts);
            }
            TEM | RST_FIRST..=RST_LAST => continue,
            SOI => {
                // A second SOI at the top level is not legal. Inside APP1 it
                // would have been skipped by that segment's length, so seeing
                // one here means we are not walking a real structure.
                return desync(i, saw_sof, saw_sos);
            }
            _ => {}
        }

        // Everything else carries a two-byte length that includes itself.
        let Some(seg_len) = be16(d, i) else {
            return truncated(
                verified_to,
                saw_sof,
                saw_sos,
                "segment length field is past the end",
            );
        };
        let seg_len = seg_len as usize;
        if seg_len < 2 {
            return desync(i, saw_sof, saw_sos);
        }
        let Some(seg_end) = i.checked_add(seg_len) else {
            return desync(i, saw_sof, saw_sos);
        };
        if seg_end > d.len() {
            return truncated(
                verified_to,
                saw_sof,
                saw_sos,
                "a segment extends past the available data",
            );
        }
        segments += 1;
        verified_to = seg_end;

        if marker == DRI {
            restart_interval = be16(d, i + 2).unwrap_or(0) as u64;
        }

        if is_sof(marker) {
            saw_sof = true;
            // SOF payload: precision(1) height(2) width(2) components(1), then
            // three bytes per component: id, sampling factors, quant table.
            if seg_len >= 8 {
                let h = be16(d, i + 3).unwrap_or(0);
                let w = be16(d, i + 5).unwrap_or(0);
                components = d.get(i + 7).copied().unwrap_or(0);
                // A single-component scan codes one block per MCU whatever the
                // declared sampling; an interleaved one codes h*v per component.
                blocks_per_mcu = if components == 1 {
                    1
                } else {
                    (0..components as usize)
                        .filter_map(|c| d.get(i + 9 + 3 * c))
                        .map(|s| ((s >> 4) as u64) * ((s & 0x0F) as u64))
                        .sum::<u64>()
                        .max(1)
                };
                if w == 0 || h == 0 {
                    return Outcome::reject("frame header declares a zero dimension");
                }
                if !matches!(components, 1 | 3 | 4) {
                    return Outcome::reject(format!(
                        "frame header declares {components} components; \
                         a JPEG has 1, 3 or 4"
                    ));
                }
                dims = Some((w, h));
            }
        }

        i = seg_end;

        if marker == SOS {
            saw_sos = true;
            // Entropy-coded data follows the scan header with no length of its
            // own. It ends at the next marker that is neither a stuffed FF 00
            // nor a restart.
            let mut k = i;
            let mut expect = 0u8;
            // The most bytes one restart interval can legitimately occupy.
            // Without DRI there is no interval and no bound.
            let max_interval = (restart_interval > 0)
                .then(|| restart_interval * blocks_per_mcu * MAX_BYTES_PER_BLOCK);
            let mut interval_start = k;
            loop {
                if let Some(limit) = max_interval {
                    let run = (k - interval_start) as u64;
                    if run > limit {
                        // Far more data between two restart markers than the
                        // declared interval can hold. This is not entropy-coded
                        // image data: the stream has been spliced into something
                        // else - the edge of a fragment, or an overwrite.
                        //
                        // Before this check the validator walked straight on,
                        // and a JPEG split into three pieces around two 32 KiB
                        // gaps came back Valid at 150,316 bytes for an 84,780-
                        // byte file. The length reported is the last restart
                        // marker, the last point the structure vouches for,
                        // which is also exactly where a reassembler has to look
                        // for the next fragment.
                        return spliced(verified_to, run, limit, restart_interval, restarts, dims);
                    }
                }
                if k + 1 >= d.len() {
                    return truncated(
                        verified_to,
                        saw_sof,
                        saw_sos,
                        "entropy-coded data runs to the end without EOI",
                    );
                }
                if d[k] != 0xFF {
                    k += 1;
                    continue;
                }
                match d[k + 1] {
                    // FF 00 is a stuffed literal FF inside the entropy data.
                    0x00 => k += 2,
                    // Fill byte; step one so the next FF is still examined.
                    0xFF => k += 1,
                    b @ RST_FIRST..=RST_LAST => {
                        if b - RST_FIRST != expect {
                            // Restart markers cycle D0..D7 strictly within a
                            // scan, so one out of sequence is not a quirk - it
                            // is data from somewhere else. Stop at the last one
                            // that was in order.
                            //
                            // This used to be counted and acted on only at EOI,
                            // while the marker still advanced `verified_to`.
                            // For a whole file that is harmless: the verdict is
                            // downgraded in the end. For reassembly it was fatal,
                            // because `verified_to` is the progress signal - a
                            // cluster of another JPEG's entropy data pushed it
                            // forward whatever its restart phase, and would have
                            // been accepted as the next fragment every time.
                            // Now it is accepted only when its phase happens to
                            // match, one time in eight.
                            return out_of_sequence(
                                verified_to,
                                expect,
                                b - RST_FIRST,
                                restarts,
                                dims,
                            );
                        }
                        restarts += 1;
                        expect = (b - RST_FIRST + 1) % 8;
                        k += 2;
                        // A restart marker is a real structural landmark, so
                        // everything up to it is justified.
                        verified_to = k;
                        interval_start = k;
                    }
                    _ => break,
                }
            }
            i = k;
        }
    }
}

/// The entropy data stopped being entropy data partway through.
///
/// Partial with an *unestablished* length: the file does continue, just not
/// here, so the length is a floor and must not be allowed to suppress other
/// candidates. The evidence carries the last good offset explicitly so a
/// reassembler does not have to parse the detail string.
fn spliced(
    last_good: usize,
    run: u64,
    limit: u64,
    interval: u64,
    restarts: u64,
    dims: Option<(u16, u16)>,
) -> Outcome {
    let (w, h) = dims.unwrap_or((0, 0));
    Outcome::partial(
        last_good as u64,
        format!(
            "{run} bytes of entropy data without a restart marker, where a {interval}-MCU \
             interval allows at most {limit}; the stream is spliced into other data - a \
             fragment boundary or an overwrite - after the last good restart marker"
        ),
    )
    .with("width", w)
    .with("height", h)
    .with("restart_markers", restarts)
    .with("restart_interval", interval)
    .with("spliced_after", last_good)
}

/// A restart marker arrived out of its strict D0..D7 cycle.
fn out_of_sequence(
    last_good: usize,
    expected: u8,
    got: u8,
    restarts: u64,
    dims: Option<(u16, u16)>,
) -> Outcome {
    let (w, h) = dims.unwrap_or((0, 0));
    Outcome::partial(
        last_good as u64,
        format!(
            "restart marker RST{got} where RST{expected} was due; restart markers cycle \
             strictly, so the entropy data is spliced from somewhere else after the last \
             in-sequence marker"
        ),
    )
    .with("width", w)
    .with("height", h)
    .with("restart_markers", restarts)
    .with("restarts_out_of_order", 1)
    .with("spliced_after", last_good)
}

/// Out of data, but we know what it is.
fn truncated(at: usize, saw_sof: bool, saw_sos: bool, why: &str) -> Outcome {
    if saw_sof && saw_sos {
        Outcome::partial(at as u64, format!("truncated: {why}")).with("truncated", true)
    } else {
        Outcome::reject(format!(
            "{why}, and no complete frame header and scan were seen first"
        ))
    }
}

/// The segment chain stopped making sense. For a real JPEG this cannot happen;
/// for random data it happens almost immediately, which is the whole point.
fn desync(at: usize, saw_sof: bool, saw_sos: bool) -> Outcome {
    if saw_sof && saw_sos {
        Outcome::partial(
            at as u64,
            "the marker chain desynchronised after the scan began",
        )
    } else {
        Outcome::reject("the marker chain does not hold together; a chance FF D8 FF")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    /// A baseline frame header for a single-component image.
    ///
    /// The declared length counts itself: 2 for the length field, then
    /// precision(1) height(2) width(2) component-count(1), then three bytes
    /// per component. For one component that is exactly 11.
    fn sof(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xFF, 0xC0, 0x00, 0x0B, 0x08];
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        assert_eq!(v.len(), 13, "SOF0 is the marker plus its declared 11 bytes");
        v
    }

    /// Scan header: length 8, one component, then the three spectral bytes.
    fn sos() -> Vec<u8> {
        vec![0xFF, 0xDA, 0x00, 0x08, 0x01, 0x00, 0x00, 0x00, 0x3F, 0x00]
    }

    /// Smallest thing that is structurally a JPEG: SOI, a frame header, a scan
    /// header, some entropy data, EOI.
    fn minimal(entropy: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        v.extend(sof(1, 1));
        v.extend(sos());
        v.extend_from_slice(entropy);
        v.extend_from_slice(&[0xFF, 0xD9]);
        v
    }

    /// A JPEG that declares a restart interval, with `intervals` restart
    /// markers each preceded by `per` bytes of entropy data. Built field by
    /// field so every segment length is right.
    fn with_dri(intervals: usize, per: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8];
        // DRI: length 4, interval 4 MCUs.
        v.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04, 0x00, 0x04]);
        // SOF0: length 11 = 2 + precision + height + width + ncomp + 3.
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
        v.extend_from_slice(&64u16.to_be_bytes()); // height
        v.extend_from_slice(&64u16.to_be_bytes()); // width
        v.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]); // 1 component, 1x1
                                                        // SOS: length 8 = 2 + ncomp + 2 + 3.
        v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        for n in 0..intervals {
            // Entropy bytes that never contain 0xFF, so no stuffing is needed.
            v.extend((0..per).map(|i| (0x11 + (i * 7 + n) % 0xEE) as u8));
            v.extend_from_slice(&[0xFF, 0xD0 + (n % 8) as u8]);
        }
        v.extend_from_slice(&[0x22, 0x33, 0xFF, 0xD9]);
        v
    }

    /// The bound must not fire on a legitimate file, or it would turn every
    /// photo with a restart interval into a false "spliced".
    #[test]
    fn a_normal_restart_interval_is_still_valid() {
        let j = with_dri(40, 64);
        let out = validate(&j);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, j.len() as u64);
        assert_eq!(out.evidence_of("restart_markers"), Some("40"));
    }

    /// The case the fragmented fixture exposed: a JPEG split around a 32 KiB
    /// gap of constant fill came back Valid, because the fill has no 0xFF in it
    /// and so never looks like a marker.
    #[test]
    fn a_long_gap_between_restart_markers_is_reported_as_a_splice() {
        let good = with_dri(10, 64);
        // Cut after the tenth restart marker, before the tail and EOI.
        let cut = good.len() - 4;
        let mut j = good[..cut].to_vec();
        let last_good = j.len();
        j.extend_from_slice(&vec![0xA5u8; 32 * 1024]);
        // The file continues on the far side of the gap, then ends normally.
        j.extend((0..64).map(|i| (0x11 + i * 3) as u8));
        j.extend_from_slice(&[0xFF, 0xD2, 0x44, 0xFF, 0xD9]);

        let out = validate(&j);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(
            !out.length_established,
            "a splice point is a floor, not an end"
        );
        assert_eq!(
            out.length, last_good as u64,
            "should stop at the last restart marker before the gap"
        );
        assert_eq!(
            out.evidence_of("spliced_after"),
            Some(last_good.to_string().as_str())
        );
        assert!(out.detail.contains("spliced"), "{}", out.detail);
    }

    /// Without DRI there is no interval to bound, so the check must not
    /// invent one. Constant fill still gets through in that case - a known gap,
    /// recorded in LIMITATIONS, that only real Huffman decoding closes.
    #[test]
    fn without_a_restart_interval_there_is_no_bound() {
        let mut j = minimal(&[0x11; 64]);
        let eoi = j.len() - 2;
        j.splice(eoi..eoi, vec![0xA5u8; 32 * 1024]);
        let out = validate(&j);
        assert_eq!(
            out.status,
            Status::Valid,
            "no DRI means nothing to measure the gap against: {}",
            out.detail
        );
    }

    #[test]
    fn accepts_a_minimal_jpeg_and_reports_its_length() {
        let j = minimal(&[0x12, 0x34]);
        let out = validate(&j);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, j.len() as u64);
        assert_eq!(out.evidence_of("width"), Some("1"));
        assert_eq!(out.evidence_of("height"), Some("1"));
    }

    #[test]
    fn stuffed_ff_inside_entropy_data_is_not_a_marker() {
        // FF 00 is a literal FF in the scan, not the end of it.
        let j = minimal(&[0xFF, 0x00, 0x12, 0xFF, 0x00]);
        let out = validate(&j);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, j.len() as u64);
    }

    #[test]
    fn counts_restart_markers_and_accepts_them_in_sequence() {
        let mut e = Vec::new();
        for n in 0..8u8 {
            e.extend_from_slice(&[0x11, 0x22]);
            e.extend_from_slice(&[0xFF, 0xD0 + n]);
        }
        let out = validate(&minimal(&e));
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.evidence_of("restart_markers"), Some("8"));
    }

    /// The signal that matters for Milestone 4: restart markers that skip are
    /// what a wrongly reassembled fragment looks like from the outside, and the
    /// scan must stop at the last one in sequence rather than walking on.
    #[test]
    fn out_of_sequence_restart_markers_stop_the_scan_at_the_splice() {
        let head = minimal(&[]);
        let scan_start = head.len() - 2; // before the EOI minimal() appends
        let e = vec![0x11, 0xFF, 0xD0, 0x22, 0xFF, 0xD5, 0x33];
        let out = validate(&minimal(&e));
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert_eq!(out.evidence_of("restarts_out_of_order"), Some("1"));
        // The last in-sequence marker is the RST0, three bytes into the scan.
        assert_eq!(out.length, (scan_start + 3) as u64);
        assert!(!out.length_established);
    }

    #[test]
    fn rejects_a_chance_header_in_random_looking_data() {
        let mut v = vec![0xFF, 0xD8, 0xFF];
        v.extend((0u8..250).map(|n| n.wrapping_mul(37).wrapping_add(11)));
        let out = validate(&v);
        assert_eq!(out.status, Status::Rejected, "{}", out.detail);
        assert_eq!(out.length, 0);
    }

    #[test]
    fn rejects_soi_eoi_with_no_frame() {
        let out = validate(&[0xFF, 0xD8, 0xFF, 0xD9]);
        assert_eq!(out.status, Status::Rejected);
    }

    #[test]
    fn rejects_a_frame_header_with_a_zero_dimension() {
        let mut v = vec![0xFF, 0xD8];
        v.extend(sof(1, 0));
        v.extend(sos());
        v.extend_from_slice(&[0x11, 0x22]);
        v.extend_from_slice(&[0xFF, 0xD9]);
        let out = validate(&v);
        assert_eq!(out.status, Status::Rejected);
        // Specifically for the dimension, not because the walk fell apart.
        assert!(out.detail.contains("zero dimension"), "{}", out.detail);
    }

    #[test]
    fn rejects_a_frame_header_with_an_impossible_component_count() {
        let mut v = vec![0xFF, 0xD8];
        let mut f = sof(4, 4);
        f[9] = 7; // component count
        v.extend(f);
        v.extend(sos());
        v.extend_from_slice(&[0xFF, 0xD9]);
        let out = validate(&v);
        assert_eq!(out.status, Status::Rejected);
        assert!(out.detail.contains("components"), "{}", out.detail);
    }

    #[test]
    fn a_truncated_jpeg_is_partial_not_rejected() {
        let full = minimal(&[0x11; 40]);
        let out = validate(&full[..full.len() - 10]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(out.length > 0);
        assert!(out.detail.contains("truncated"), "{}", out.detail);
    }

    /// An EXIF thumbnail is a whole JPEG inside APP1 of the outer one. Walking
    /// APP1 by its declared length must step over it, so the outer file's
    /// length is the outer file's, not the thumbnail's.
    #[test]
    fn steps_over_an_exif_thumbnail_rather_than_ending_at_its_eoi() {
        let thumb = minimal(&[0xAB, 0xCD]);
        let mut v = vec![0xFF, 0xD8];
        let app1_len = thumb.len() + 2;
        v.extend_from_slice(&[0xFF, 0xE1]);
        v.extend_from_slice(&(app1_len as u16).to_be_bytes());
        v.extend_from_slice(&thumb);
        v.extend(sof(2, 2));
        v.extend(sos());
        v.extend_from_slice(&[0x55, 0x66]);
        v.extend_from_slice(&[0xFF, 0xD9]);

        let out = validate(&v);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, v.len() as u64, "ended at the thumbnail's EOI");
        assert_eq!(out.evidence_of("width"), Some("2"));
    }
}
