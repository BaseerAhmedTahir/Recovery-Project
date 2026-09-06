//! PDF validator: `%PDF-` to the *right* `%%EOF`, with the xref chain checked.
//!
//! Two things make PDF harder to carve than its header and footer suggest.
//!
//! First, a PDF that has been edited carries several `%%EOF` markers, one per
//! incremental update, and the real end of the file is the last of them.
//! Taking the first produces a file that opens, shows the original revision,
//! and silently discards every later edit - a corruption that validates.
//!
//! Second, taking *the* last `%%EOF` in the read window is just as wrong when
//! the next file on the disk is also a PDF, because then the carve swallows
//! its neighbour. So the search is bounded at the next `%PDF-` header.
//!
//! What raises this above string-matching is `startxref`: the trailer names
//! the byte offset of the cross-reference table, and that offset has to land
//! on something that actually is one. Random data does not satisfy that.

use super::Outcome;
use memchr::memmem;

const HEADER: &[u8] = b"%PDF-";
const EOF_MARK: &[u8] = b"%%EOF";
const STARTXREF: &[u8] = b"startxref";

pub fn validate(d: &[u8]) -> Outcome {
    if d.len() < 8 {
        return Outcome::reject("shorter than a PDF header");
    }
    if !d.starts_with(HEADER) {
        return Outcome::reject("does not begin with %PDF-");
    }
    // %PDF-1.0 through %PDF-2.0 are the versions that exist.
    let version = &d[5..8];
    if !(version[0].is_ascii_digit() && version[1] == b'.' && version[2].is_ascii_digit()) {
        return Outcome::reject("the version after %PDF- is not N.N");
    }
    let version = String::from_utf8_lossy(version).to_string();

    // Bound the search at the next file's header so a carve cannot absorb an
    // adjacent PDF. Searching from byte 1 so our own header is not the hit.
    let bound = memmem::find(&d[1..], HEADER)
        .map(|p| p + 1)
        .unwrap_or(d.len());

    let eofs: Vec<usize> = memmem::find_iter(&d[..bound], EOF_MARK).collect();
    if eofs.is_empty() {
        // No trailer at all. Still a PDF if it has objects; just not a whole
        // one. Requiring "obj" keeps random data out.
        return if memmem::find(&d[..bound.min(d.len())], b" obj").is_some() {
            Outcome::partial(bound as u64, "no %%EOF trailer; the file is truncated")
                .with("version", version)
                .with("truncated", true)
        } else {
            Outcome::reject("no %%EOF and no object definitions")
        };
    }

    let last = *eofs.last().unwrap();
    // Include the marker and whatever line ending follows it.
    let mut end = last + EOF_MARK.len();
    while end < d.len() && (d[end] == b'\r' || d[end] == b'\n') {
        end += 1;
    }

    // The trailer immediately before that marker must name the xref offset.
    let search_from = last.saturating_sub(512);
    let startxref = memmem::rfind(&d[search_from..last], STARTXREF).map(|p| p + search_from);

    let Some(sx) = startxref else {
        return Outcome::partial(end as u64, "the final trailer has no startxref")
            .with("version", version)
            .with("eof_markers", eofs.len());
    };

    let offset = parse_offset(&d[sx + STARTXREF.len()..last]);
    let Some(offset) = offset else {
        return Outcome::partial(end as u64, "startxref is not followed by a number")
            .with("version", version)
            .with("eof_markers", eofs.len());
    };

    // The offset is relative to the start of the file, which for a carved
    // candidate is exactly byte zero of our slice.
    let target_ok = match d.get(offset as usize..) {
        None => false,
        Some(t) => {
            // Either a classic cross-reference table, or a cross-reference
            // stream, which begins with an indirect object header.
            t.starts_with(b"xref") || looks_like_object_header(t)
        }
    };

    let out = Outcome::valid(end as u64)
        .with("version", version.clone())
        .with("eof_markers", eofs.len())
        .with("startxref", offset)
        .with("incremental_updates", eofs.len().saturating_sub(1));

    if !target_ok {
        // The structure is a PDF but its index does not point at an index.
        // Recoverable, and the viewer will have to rebuild the xref.
        return Outcome::partial(
            end as u64,
            format!("startxref points to {offset}, which is not an xref table or stream"),
        )
        .with("version", version)
        .with("eof_markers", eofs.len())
        .with("startxref", offset);
    }
    out
}

/// `123 0 obj` - the header of an indirect object, which is what a
/// cross-reference stream starts with.
fn looks_like_object_header(t: &[u8]) -> bool {
    let mut i = 0;
    let mut digits = 0;
    while i < t.len() && t[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if digits == 0 || i >= t.len() || t[i] != b' ' {
        return false;
    }
    i += 1;
    let mut gen = 0;
    while i < t.len() && t[i].is_ascii_digit() {
        i += 1;
        gen += 1;
    }
    gen > 0 && t[i..].starts_with(b" obj")
}

fn parse_offset(t: &[u8]) -> Option<u64> {
    let mut i = 0;
    while i < t.len() && (t[i] == b'\r' || t[i] == b'\n' || t[i] == b' ' || t[i] == b'\t') {
        i += 1;
    }
    let start = i;
    let mut v: u64 = 0;
    while i < t.len() && t[i].is_ascii_digit() {
        v = v.checked_mul(10)?.checked_add((t[i] - b'0') as u64)?;
        i += 1;
    }
    if i == start {
        None
    } else {
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    /// Build a PDF whose startxref genuinely points at its xref table.
    fn pdf(revisions: usize) -> Vec<u8> {
        let mut v = b"%PDF-1.7\n".to_vec();
        v.extend_from_slice(b"1 0 obj\n<< /Type /Catalog >>\nendobj\n");
        for _ in 0..revisions {
            let xref_at = v.len();
            v.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
            v.extend_from_slice(b"trailer\n<< /Size 1 /Root 1 0 R >>\nstartxref\n");
            v.extend_from_slice(format!("{xref_at}\n").as_bytes());
            v.extend_from_slice(b"%%EOF\n");
        }
        v
    }

    #[test]
    fn accepts_a_single_revision_pdf() {
        let p = pdf(1);
        let out = validate(&p);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, p.len() as u64);
        assert_eq!(out.evidence_of("version"), Some("1.7"));
        assert_eq!(out.evidence_of("incremental_updates"), Some("0"));
    }

    /// The failure that produces a file which opens and is quietly wrong.
    #[test]
    fn takes_the_last_eof_so_incremental_updates_are_not_discarded() {
        let p = pdf(3);
        let out = validate(&p);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, p.len() as u64);
        assert_eq!(out.evidence_of("eof_markers"), Some("3"));
        assert_eq!(out.evidence_of("incremental_updates"), Some("2"));
    }

    /// The opposite failure: two PDFs adjacent on disk, and the carve of the
    /// first swallows the second.
    #[test]
    fn stops_at_the_next_pdf_header_rather_than_absorbing_it() {
        let first = pdf(1);
        let second = pdf(2);
        let mut both = first.clone();
        both.extend_from_slice(&second);
        let out = validate(&both);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, first.len() as u64);
    }

    #[test]
    fn trailing_disk_content_does_not_extend_the_length() {
        let mut p = pdf(1);
        let real = p.len();
        p.extend_from_slice(&[0x00; 8192]);
        assert_eq!(validate(&p).length, real as u64);
    }

    #[test]
    fn a_pdf_with_objects_but_no_trailer_is_partial() {
        let mut v = b"%PDF-1.4\n".to_vec();
        v.extend_from_slice(b"1 0 obj\n<< /Type /Catalog >>\nendobj\n");
        let out = validate(&v);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(out.detail.contains("truncated"), "{}", out.detail);
    }

    #[test]
    fn a_startxref_pointing_nowhere_is_partial_rather_than_valid() {
        let mut p = pdf(1);
        let at = memmem::rfind(&p, b"startxref\n").unwrap() + b"startxref\n".len();
        // Point it into the middle of the catalog object instead.
        p[at..at + 2].copy_from_slice(b"12");
        let out = validate(&p);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(out.detail.contains("not an xref"), "{}", out.detail);
    }

    #[test]
    fn rejects_a_chance_header() {
        assert_eq!(validate(b"%PDF-XY nonsense here").status, Status::Rejected);
        let mut v = b"%PDF-1.4".to_vec();
        v.extend((0u8..200).map(|n| n.wrapping_mul(53)));
        assert_eq!(validate(&v).status, Status::Rejected);
    }

    #[test]
    fn accepts_a_cross_reference_stream_target() {
        let mut v = b"%PDF-2.0\n".to_vec();
        v.extend_from_slice(b"1 0 obj\n<< >>\nendobj\n");
        let xref_at = v.len();
        v.extend_from_slice(b"2 0 obj\n<< /Type /XRef >>\nstream\nendstream\nendobj\n");
        v.extend_from_slice(b"startxref\n");
        v.extend_from_slice(format!("{xref_at}\n").as_bytes());
        v.extend_from_slice(b"%%EOF");
        let out = validate(&v);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
    }
}
