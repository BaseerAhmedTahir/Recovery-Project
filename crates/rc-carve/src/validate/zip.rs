//! ZIP validator, and the OOXML sniffing that keeps `.docx` from being `.zip`.
//!
//! Every OOXML document, every OpenDocument file, every EPUB, every JAR and
//! every APK is a ZIP. A carver that reports the container format is
//! technically correct and practically useless: a recovery of someone's
//! documents folder comes back as four hundred files called `00001.zip`.
//!
//! So this validator walks the central directory, reads the member names, and
//! names the format from them. `[Content_Types].xml` as a member is the OOXML
//! marker, and the top-level directory - `word/`, `xl/`, `ppt/` - says which
//! OOXML format it is.
//!
//! The other job is finding the right End of Central Directory record. The
//! carve window runs to `max_size`, so it may contain several archives. The
//! EOCD names the offset of the central directory relative to the start of its
//! own archive, so the correct EOCD is the first one whose offset lands on a
//! central directory header in this slice. Taking the last EOCD in the window
//! would merge an archive with its neighbours.

use super::{le16, le32, Outcome};
use crate::crc32::crc32;
use memchr::memmem;

const LOCAL: &[u8] = b"PK\x03\x04";
const CENTRAL: &[u8] = b"PK\x01\x02";
const EOCD: &[u8] = b"PK\x05\x06";
const EOCD64: &[u8] = b"PK\x06\x06";
const EOCD_LEN: usize = 22;
/// Value both offset fields take when the real one lives in the ZIP64 record.
const ZIP64_MARKER: u32 = 0xFFFF_FFFF;

pub fn validate(d: &[u8]) -> Outcome {
    if d.len() < LOCAL.len() {
        return Outcome::reject("shorter than a ZIP local file header");
    }
    if !d.starts_with(LOCAL) {
        return Outcome::reject("does not begin with a local file header");
    }

    // Try each EOCD in order and keep the first that describes *this* archive.
    for pos in memmem::find_iter(d, EOCD) {
        let Some(record) = read_eocd(d, pos) else {
            continue;
        };
        if record.cd_offset == ZIP64_MARKER || record.entries == u16::MAX {
            // A ZIP64 archive. The 32-bit fields are placeholders, so the real
            // directory offset is in the ZIP64 EOCD record.
            if let Some(out) = zip64(d, pos, &record) {
                return out;
            }
            continue;
        }
        let cd_at = record.cd_offset as usize;
        if cd_at >= d.len() || !d[cd_at..].starts_with(CENTRAL) {
            continue; // an EOCD belonging to some other archive in the window
        }
        if cd_at + record.cd_size as usize > d.len() {
            continue;
        }
        let end = pos + EOCD_LEN + record.comment_len as usize;
        if end > d.len() {
            return Outcome::partial(d.len() as u64, "the EOCD comment runs past the data");
        }
        let members = read_central_directory(d, cd_at, record.entries as usize);
        return classify(d, end as u64, record.entries, &members, false);
    }

    // No usable EOCD. If local headers chain, it is a truncated archive rather
    // than a chance PK\x03\x04.
    let locals = memmem::find_iter(d, LOCAL).count();
    if locals >= 1 && plausible_local_header(d) {
        // Report the extent the local-header chain actually justifies, NOT
        // `d.len()`. Returning the size of the buffer means returning the
        // caller's window size, which is a fact about the scanner rather than
        // about the file - and a candidate carrying a 16 MiB length it did not
        // earn will swallow every real file inside that span.
        let extent = walked_local_extent(d);
        Outcome::partial(
            extent,
            "no end-of-central-directory record; the archive is truncated and its \
             member index is gone, so this length is what the local headers \
             justify rather than the file's real size",
        )
        .with("local_headers", locals)
        .with("truncated", true)
    } else {
        Outcome::reject("no central directory and the local header is not plausible")
    }
}

/// How far the chain of local file headers can be followed.
///
/// Each local header declares its member's compressed size, so a run of them
/// gives a lower bound on the archive's size that comes from the data rather
/// than from how much of it we happened to read. Stops at the first header that
/// does not add up; a member written with a data descriptor has a zero size
/// here and ends the walk, which is the conservative direction.
fn walked_local_extent(d: &[u8]) -> u64 {
    let mut at = 0usize;
    let mut last_good = 0u64;
    while at + 30 <= d.len() && d[at..].starts_with(LOCAL) {
        let csize = le32(d, at + 18).unwrap_or(0) as usize;
        let name_len = le16(d, at + 26).unwrap_or(0) as usize;
        let extra_len = le16(d, at + 28).unwrap_or(0) as usize;
        let next = at + 30 + name_len + extra_len + csize;
        if csize == 0 || next > d.len() || next <= at {
            // Header present but its body is not, or its size is unknown.
            // Credit the header itself and stop.
            last_good = (at + 30 + name_len + extra_len).min(d.len()) as u64;
            break;
        }
        last_good = next as u64;
        at = next;
    }
    last_good
}

struct Eocd {
    entries: u16,
    cd_size: u32,
    cd_offset: u32,
    comment_len: u16,
}

fn read_eocd(d: &[u8], at: usize) -> Option<Eocd> {
    if at + EOCD_LEN > d.len() {
        return None;
    }
    Some(Eocd {
        entries: le16(d, at + 10)?,
        cd_size: le32(d, at + 12)?,
        cd_offset: le32(d, at + 16)?,
        comment_len: le16(d, at + 20)?,
    })
}

/// ZIP64: the 32-bit fields are saturated and the real ones live in a separate
/// record ahead of the EOCD.
fn zip64(d: &[u8], eocd_at: usize, record: &Eocd) -> Option<Outcome> {
    let z64 = memmem::rfind(&d[..eocd_at], EOCD64)?;
    let entries = super::le64(d, z64 + 32)?;
    let cd_offset = super::le64(d, z64 + 48)?;
    let cd_at = usize::try_from(cd_offset).ok()?;
    if cd_at >= d.len() || !d[cd_at..].starts_with(CENTRAL) {
        return None;
    }
    let end = eocd_at + EOCD_LEN + record.comment_len as usize;
    if end > d.len() {
        return None;
    }
    let members = read_central_directory(d, cd_at, entries.min(u16::MAX as u64) as usize);
    Some(classify(
        d,
        end as u64,
        entries.min(u16::MAX as u64) as u16,
        &members,
        true,
    ))
}

/// One central-directory entry, reduced to what this validator needs.
struct Member {
    name: String,
    /// 0 is stored, 8 is deflate.
    method: u16,
    crc: u32,
    compressed_size: u32,
    local_offset: u32,
}

/// Walk the central directory and collect its members.
fn read_central_directory(d: &[u8], mut at: usize, entries: usize) -> Vec<Member> {
    let mut members = Vec::new();
    // Cap the walk: `entries` comes off the disk and cannot be trusted to be
    // small, and a corrupt value must not turn into an enormous allocation.
    let cap = entries.min(65_535);
    for _ in 0..cap {
        if at + 46 > d.len() || !d[at..].starts_with(CENTRAL) {
            break;
        }
        let name_len = le16(d, at + 28).unwrap_or(0) as usize;
        let extra_len = le16(d, at + 30).unwrap_or(0) as usize;
        let comment_len = le16(d, at + 32).unwrap_or(0) as usize;
        let name_at = at + 46;
        if name_at + name_len > d.len() {
            break;
        }
        members.push(Member {
            name: String::from_utf8_lossy(&d[name_at..name_at + name_len]).to_string(),
            method: le16(d, at + 10).unwrap_or(0xFFFF),
            crc: le32(d, at + 16).unwrap_or(0),
            compressed_size: le32(d, at + 20).unwrap_or(0),
            local_offset: le32(d, at + 42).unwrap_or(0),
        });
        at = name_at + name_len + extra_len + comment_len;
    }
    members
}

/// Verify the checksums of members stored without compression.
///
/// ZIP records a CRC-32 per member, which is the only content-level evidence
/// the format offers - everything else here is structure. A deflated member
/// cannot be checked without decompressing it, and pulling a decompressor into
/// the carve path is not a trade worth making, so this checks what it can and
/// reports how much that was rather than implying it checked everything.
///
/// Returns `(verified, failed)`.
fn verify_stored_members(d: &[u8], members: &[Member]) -> (u64, u64) {
    let mut verified = 0u64;
    let mut failed = 0u64;
    for m in members {
        if m.method != 0 || m.compressed_size == 0 {
            continue;
        }
        let lo = m.local_offset as usize;
        if lo + 30 > d.len() || !d[lo..].starts_with(LOCAL) {
            continue;
        }
        // The local header repeats the name and extra lengths, and they are
        // allowed to differ from the central directory copy, so read them from
        // the local header rather than reusing what we already have.
        let name_len = le16(d, lo + 26).unwrap_or(0) as usize;
        let extra_len = le16(d, lo + 28).unwrap_or(0) as usize;
        let body = lo + 30 + name_len + extra_len;
        let Some(end) = body.checked_add(m.compressed_size as usize) else {
            continue;
        };
        if end > d.len() {
            continue;
        }
        if crc32(&d[body..end]) == m.crc {
            verified += 1;
        } else {
            failed += 1;
        }
    }
    (verified, failed)
}

/// Decide what this archive actually is from its member names.
fn classify(d: &[u8], length: u64, entries: u16, members: &[Member], zip64: bool) -> Outcome {
    let has = |n: &str| members.iter().any(|m| m.name == n);
    let under = |p: &str| members.iter().any(|m| m.name.starts_with(p));

    let (ext, what) = if has("[Content_Types].xml") {
        // OOXML. The part tree says which application wrote it.
        if under("word/") {
            ("docx", "OOXML word processing document")
        } else if under("xl/") {
            ("xlsx", "OOXML spreadsheet")
        } else if under("ppt/") {
            ("pptx", "OOXML presentation")
        } else {
            ("zip", "OOXML package of an unrecognised kind")
        }
    } else if has("META-INF/MANIFEST.MF") {
        if has("AndroidManifest.xml") {
            ("apk", "Android package")
        } else {
            ("jar", "Java archive")
        }
    } else if has("mimetype") {
        // ODF and EPUB both store an uncompressed `mimetype` member first.
        if under("OEBPS/") || has("META-INF/container.xml") {
            ("epub", "EPUB publication")
        } else {
            ("odf", "OpenDocument package")
        }
    } else {
        ("zip", "ZIP archive")
    };

    let (crc_ok, crc_bad) = verify_stored_members(d, members);

    let evidence = |o: Outcome| {
        o.with_ext(ext)
            .with("entries", entries)
            .with("names_read", members.len())
            .with("zip64", zip64)
            .with("stored_members_verified", crc_ok)
            .with("stored_members_failed", crc_bad)
    };

    // A member whose checksum fails is content-level damage, which the
    // structure walk on its own cannot see.
    if crc_bad > 0 {
        return evidence(Outcome::partial(
            length,
            format!(
                "{crc_bad} stored member(s) fail their CRC-32; the archive is complete \
                 but its contents are partially overwritten; {what}"
            ),
        ));
    }

    // The directory promised more members than we could walk to. The archive
    // is there but its tail is damaged.
    if (members.len() as u16) < entries {
        return evidence(Outcome::partial(
            length,
            format!(
                "the central directory declares {entries} members but only {} could be read; \
                 {what}",
                members.len()
            ),
        ));
    }

    evidence(Outcome::valid(length)).note(what.to_string())
}

/// Sanity-check the first local file header so a chance `PK\x03\x04` in noise
/// is not reported as a truncated archive.
fn plausible_local_header(d: &[u8]) -> bool {
    if d.len() < 30 {
        return false;
    }
    let method = le16(d, 8).unwrap_or(0xFFFF);
    let name_len = le16(d, 26).unwrap_or(0) as usize;
    // 0 is stored, 8 is deflate; the rest are rare but defined up to 99.
    let method_ok = matches!(method, 0 | 1 | 6 | 8 | 9 | 12 | 14 | 93..=99);
    let name_ok = name_len > 0 && name_len <= 4096 && 30 + name_len <= d.len();
    // A member name is text, not arbitrary bytes.
    let name_is_text = name_ok
        && d[30..30 + name_len]
            .iter()
            .all(|b| *b >= 0x20 || *b == b'/' || *b == b'\\');
    method_ok && name_is_text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc32::crc32;
    use crate::validate::Status;

    /// Minimal stored-method ZIP writer, so the tests do not depend on a
    /// compression library to build their fixtures.
    fn zip(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in members {
            let offset = out.len() as u32;
            let crc = crc32(data);
            out.extend_from_slice(LOCAL);
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // ver/flags/method/time
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            central.extend_from_slice(CENTRAL);
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(EOCD);
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn docx() -> Vec<u8> {
        zip(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("_rels/.rels", b"<Relationships/>"),
            ("word/document.xml", b"<document/>"),
        ])
    }

    #[test]
    fn accepts_a_plain_zip() {
        let z = zip(&[("a.txt", b"hello"), ("b.txt", b"world")]);
        let out = validate(&z);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, z.len() as u64);
        assert_eq!(out.refined_ext, Some("zip"));
        assert_eq!(out.evidence_of("entries"), Some("2"));
    }

    /// The headline reason this validator exists.
    #[test]
    fn an_ooxml_package_is_reported_as_docx_not_zip() {
        let out = validate(&docx());
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.refined_ext, Some("docx"));
    }

    #[test]
    fn distinguishes_the_three_ooxml_applications() {
        for (dir, want) in [("word/", "docx"), ("xl/", "xlsx"), ("ppt/", "pptx")] {
            let part = format!("{dir}main.xml");
            let z = zip(&[("[Content_Types].xml", b"<Types/>"), (&part, b"<x/>")]);
            assert_eq!(validate(&z).refined_ext, Some(want), "for {dir}");
        }
    }

    #[test]
    fn recognises_jar_apk_and_epub() {
        let jar = zip(&[("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0")]);
        assert_eq!(validate(&jar).refined_ext, Some("jar"));

        let apk = zip(&[
            ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0"),
            ("AndroidManifest.xml", b"\x03\x00\x08\x00"),
        ]);
        assert_eq!(validate(&apk).refined_ext, Some("apk"));

        let epub = zip(&[
            ("mimetype", b"application/epub+zip"),
            ("META-INF/container.xml", b"<container/>"),
        ]);
        assert_eq!(validate(&epub).refined_ext, Some("epub"));
    }

    /// Two archives adjacent on disk. Taking the last EOCD in the window would
    /// report one file spanning both.
    #[test]
    fn stops_at_its_own_eocd_rather_than_a_later_archive() {
        let first = docx();
        let mut both = first.clone();
        both.extend_from_slice(&zip(&[("other.txt", b"xx")]));
        let out = validate(&both);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, first.len() as u64);
        assert_eq!(out.refined_ext, Some("docx"));
    }

    #[test]
    fn trailing_bytes_do_not_extend_the_length() {
        let mut z = docx();
        let real = z.len();
        z.extend_from_slice(&[0x00; 2048]);
        assert_eq!(validate(&z).length, real as u64);
    }

    #[test]
    fn a_zip_with_no_central_directory_is_partial() {
        let z = docx();
        let cut = memmem::find(&z, CENTRAL).unwrap();
        let out = validate(&z[..cut]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(out.detail.contains("truncated"), "{}", out.detail);
        // The length must come from the local-header chain, not from how much
        // data the caller happened to hand over.
        assert!(out.length > 0 && out.length <= cut as u64);
    }

    /// The bug that destroyed recall on the quick-formatted fixture: a
    /// truncated archive reported the caller's buffer size as its length, so
    /// one spurious PK\x03\x04 claimed the whole validation window.
    #[test]
    fn a_truncated_archive_does_not_report_the_buffer_size_as_its_length() {
        let z = docx();
        let cut = memmem::find(&z, CENTRAL).unwrap();
        let head = &z[..cut];

        // Same prefix, three different amounts of trailing padding. The
        // reported length must not move.
        let mut lengths = Vec::new();
        for pad in [0usize, 4096, 1 << 20] {
            let mut v = head.to_vec();
            v.resize(head.len() + pad, 0);
            let out = validate(&v);
            assert_eq!(out.status, Status::Partial, "{}", out.detail);
            lengths.push(out.length);
        }
        assert!(
            lengths.iter().all(|l| *l == lengths[0]),
            "length changed with the size of the buffer: {lengths:?}"
        );
        assert!(
            lengths[0] <= head.len() as u64,
            "claimed {} bytes from a {}-byte archive prefix",
            lengths[0],
            head.len()
        );
    }

    /// ZIP's per-member CRC is the only content-level check the format offers,
    /// and it is the difference between "the index parses" and "the bytes are
    /// intact". The test writer above stores everything uncompressed, so every
    /// member is checkable.
    #[test]
    fn verifies_the_checksums_of_stored_members() {
        let z = zip(&[("a.txt", b"hello"), ("b.txt", b"world")]);
        let out = validate(&z);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.evidence_of("stored_members_verified"), Some("2"));
        assert_eq!(out.evidence_of("stored_members_failed"), Some("0"));
    }

    #[test]
    fn a_member_whose_content_was_overwritten_is_partial() {
        let mut z = zip(&[("a.txt", b"hello there"), ("b.txt", b"world")]);
        // Corrupt a byte of the first member's payload, leaving every length
        // field and the whole central directory intact.
        let at = memmem::find(&z, b"hello there").unwrap();
        z[at + 2] ^= 0xFF;
        let out = validate(&z);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert_eq!(out.evidence_of("stored_members_failed"), Some("1"));
        assert_eq!(out.evidence_of("stored_members_verified"), Some("1"));
        assert!(out.detail.contains("CRC-32"), "{}", out.detail);
    }

    #[test]
    fn rejects_a_chance_pk_signature() {
        let mut v = LOCAL.to_vec();
        v.extend((0u8..200).map(|n| n.wrapping_mul(97).wrapping_add(3)));
        assert_eq!(validate(&v).status, Status::Rejected);
    }

    #[test]
    fn a_central_directory_short_of_its_declared_members_is_partial() {
        let mut z = zip(&[("a.txt", b"1"), ("b.txt", b"2")]);
        let eocd = memmem::rfind(&z, EOCD).unwrap();
        // Claim four members where two exist.
        z[eocd + 10..eocd + 12].copy_from_slice(&4u16.to_le_bytes());
        let out = validate(&z);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert_eq!(out.evidence_of("names_read"), Some("2"));
    }
}
