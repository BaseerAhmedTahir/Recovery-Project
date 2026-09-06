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
    let mut restarts_out_of_order = 0u64;
    let mut segments = 0u64;

    loop {
        // Fill bytes: a stream may pad with any number of FFs before a marker.
        let mut j = i;
        while j < d.len() && d[j] == 0xFF {
            j += 1;
        }
        if j >= d.len() {
            return truncated(i, saw_sof, saw_sos, "ran out of data looking for a marker");
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
                let mut out = Outcome::valid(len)
                    .with("width", w)
                    .with("height", h)
                    .with("components", components)
                    .with("segments", segments)
                    .with("restart_markers", restarts);
                if restarts_out_of_order > 0 {
                    // Restart markers cycle D0..D7 strictly. Out-of-sequence
                    // ones mean the entropy data is spliced - which is exactly
                    // what a wrongly reassembled fragment looks like.
                    out = Outcome::partial(
                        len,
                        format!(
                            "{restarts_out_of_order} restart marker(s) out of sequence; \
                             the entropy data is probably spliced from the wrong fragments"
                        ),
                    )
                    .with("width", w)
                    .with("height", h)
                    .with("components", components)
                    .with("restart_markers", restarts)
                    .with("restarts_out_of_order", restarts_out_of_order);
                }
                return out;
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
            return truncated(i, saw_sof, saw_sos, "segment length field is past the end");
        };
        let seg_len = seg_len as usize;
        if seg_len < 2 {
            return desync(i, saw_sof, saw_sos);
        }
        let Some(seg_end) = i.checked_add(seg_len) else {
            return desync(i, saw_sof, saw_sos);
        };
        if seg_end > d.len() {
            return truncated(i, saw_sof, saw_sos, "a segment extends past the available data");
        }
        segments += 1;

        if is_sof(marker) {
            saw_sof = true;
            // SOF payload: precision(1) height(2) width(2) components(1).
            if seg_len >= 8 {
                let h = be16(d, i + 3).unwrap_or(0);
                let w = be16(d, i + 5).unwrap_or(0);
                components = d.get(i + 7).copied().unwrap_or(0);
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
            loop {
                if k + 1 >= d.len() {
                    return truncated(
                        d.len(),
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
                        restarts += 1;
                        if b - RST_FIRST != expect {
                            restarts_out_of_order += 1;
                        }
                        expect = (b - RST_FIRST + 1) % 8;
                        k += 2;
                    }
                    _ => break,
                }
            }
            i = k;
        }
    }
}

/// Out of data, but we know what it is.
fn truncated(at: usize, saw_sof: bool, saw_sos: bool, why: &str) -> Outcome {
    if saw_sof && saw_sos {
        Outcome::partial(at as u64, format!("truncated: {why}"))
            .with("truncated", true)
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
        Outcome::partial(at as u64, "the marker chain desynchronised after the scan began")
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
    /// what a wrongly reassembled fragment looks like from the outside.
    #[test]
    fn out_of_sequence_restart_markers_downgrade_to_partial() {
        let e = vec![0x11, 0xFF, 0xD0, 0x22, 0xFF, 0xD5, 0x33];
        let out = validate(&minimal(&e));
        assert_eq!(out.status, Status::Partial);
        assert_eq!(out.evidence_of("restarts_out_of_order"), Some("1"));
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
