//! SQLite validator: header field cross-checks, page count, freelist sanity.
//!
//! The 16-byte magic makes false positives rare, so this validator is not
//! really about rejection. It is about **length** and **trustworthiness**,
//! both of which matter more here than for any other format in the database,
//! because SQLite files are the target of Milestone 7's deleted-row recovery.
//!
//! Length comes from the header: page size times page count. But the page
//! count in the header is only meaningful when the "version valid for" counter
//! matches the change counter - SQLite says so explicitly, and a file written
//! by an older library, or one recovered mid-write, will disagree. Carving
//! `page_size * page_count` regardless would silently truncate or overrun.
//!
//! The freelist is checked because it is the thing `rc-sqlite-carve` will read
//! later: deleted rows survive in free pages. A freelist whose trunk page lies
//! outside the database, or which claims more free pages than the database
//! has, means those pages cannot be trusted, and it is better to say that now
//! than to hand Milestone 7 a corrupt chain.

use super::{be32, Outcome};

pub const MAGIC: &[u8; 16] = b"SQLite format 3\x00";
const HEADER_LEN: usize = 100;

pub fn validate(d: &[u8]) -> Outcome {
    if d.len() < HEADER_LEN {
        return Outcome::reject("shorter than a SQLite header");
    }
    if &d[..16] != MAGIC {
        return Outcome::reject("magic mismatch");
    }

    // Offset 16: page size. The stored 1 means 65536, which does not fit the
    // 16-bit field.
    let raw_page = u16::from_be_bytes([d[16], d[17]]);
    let page_size: u32 = if raw_page == 1 {
        65536
    } else {
        raw_page as u32
    };
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return Outcome::reject(format!(
            "page size {page_size} is not a power of two between 512 and 65536"
        ));
    }

    let write_version = d[18];
    let read_version = d[19];
    if !matches!(write_version, 1 | 2) || !matches!(read_version, 1 | 2) {
        return Outcome::reject("file format version bytes are not 1 or 2");
    }

    // Offsets 21-23 are fixed constants in every SQLite file ever written.
    // They are the cheapest strong check in the header.
    if d[21] != 64 || d[22] != 32 || d[23] != 32 {
        return Outcome::reject(
            "the payload-fraction bytes are not 64/32/32; every SQLite file has these",
        );
    }

    let reserved = d[20] as u32;
    let usable = page_size.saturating_sub(reserved);
    if usable < 480 {
        return Outcome::reject(format!(
            "reserved space leaves {usable} usable bytes per page; the minimum is 480"
        ));
    }

    let change_counter = be32(d, 24).unwrap_or(0);
    let page_count = be32(d, 28).unwrap_or(0);
    let freelist_trunk = be32(d, 32).unwrap_or(0);
    let freelist_pages = be32(d, 36).unwrap_or(0);
    let schema_format = be32(d, 44).unwrap_or(0);
    let text_encoding = be32(d, 56).unwrap_or(0);
    let version_valid_for = be32(d, 92).unwrap_or(0);
    let library_version = be32(d, 96).unwrap_or(0);

    if schema_format != 0 && !matches!(schema_format, 1..=4) {
        return Outcome::reject(format!("schema format {schema_format} is not 1-4"));
    }
    if text_encoding != 0 && !matches!(text_encoding, 1..=3) {
        return Outcome::reject(format!("text encoding {text_encoding} is not 1-3"));
    }
    // Bytes 72..91 are reserved for expansion and must be zero.
    if d[72..92].iter().any(|b| *b != 0) {
        return Outcome::reject("the reserved header range 72..92 is not zero");
    }

    // Page 1 carries a b-tree page header immediately after the file header.
    let page1_type = d[HEADER_LEN];
    if !matches!(page1_type, 0x02 | 0x05 | 0x0A | 0x0D) {
        return Outcome::reject(format!(
            "page 1 declares b-tree type 0x{page1_type:02X}, which is not a page type"
        ));
    }

    let mut notes: Vec<String> = Vec::new();

    // Freelist sanity. Only meaningful once we believe the page count.
    let size_is_trustworthy = page_count != 0 && version_valid_for == change_counter;
    if size_is_trustworthy {
        if freelist_pages >= page_count {
            return Outcome::reject(format!(
                "the freelist claims {freelist_pages} free pages in a {page_count}-page database"
            ));
        }
        if freelist_trunk > page_count {
            return Outcome::reject(format!(
                "the freelist trunk is page {freelist_trunk} in a {page_count}-page database"
            ));
        }
    }
    if (freelist_trunk == 0) != (freelist_pages == 0) {
        // One says there is a freelist and the other says there is not.
        notes.push(format!(
            "freelist head and count disagree (trunk page {freelist_trunk}, \
             {freelist_pages} pages); free-page recovery on this file is unreliable"
        ));
    }

    let evidence = |o: Outcome| {
        o.with("page_size", page_size)
            .with("page_count", page_count)
            .with("freelist_pages", freelist_pages)
            .with("freelist_trunk", freelist_trunk)
            .with("library_version", library_version)
            .with("text_encoding", text_encoding)
            .with("size_in_header_valid", size_is_trustworthy)
    };

    if !size_is_trustworthy {
        // Without a trustworthy page count there is no length in the file.
        // Fall back to whole pages of what we actually have, and say so rather
        // than inventing a size.
        let whole = (d.len() as u64 / page_size as u64) * page_size as u64;
        if whole == 0 {
            return Outcome::reject("less than one page of data is present");
        }
        return evidence(Outcome::partial(
            whole,
            "the in-header database size is not valid (version-valid-for does not match \
             the change counter), so the length is a floor of whole pages, not the \
             file's own claim",
        ));
    }

    let declared = page_size as u64 * page_count as u64;
    if declared > d.len() as u64 {
        let whole = (d.len() as u64 / page_size as u64) * page_size as u64;
        return evidence(Outcome::partial(
            whole,
            format!(
                "the header declares {page_count} pages ({declared} bytes) but only {} \
                 are available; truncated",
                d.len()
            ),
        )
        .with("truncated", true));
    }

    let mut out = evidence(Outcome::valid(declared));
    if !notes.is_empty() {
        out = evidence(Outcome::partial(declared, notes.join("; ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    /// Build a header-accurate SQLite file. Only the header and page 1's type
    /// byte matter to this validator, so the page bodies are filler.
    fn db(page_size: u32, pages: u32, freelist: u32, trunk: u32, valid_size: bool) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_LEN];
        h[..16].copy_from_slice(MAGIC);
        let raw = if page_size == 65536 { 1u16 } else { page_size as u16 };
        h[16..18].copy_from_slice(&raw.to_be_bytes());
        h[18] = 1;
        h[19] = 1;
        h[20] = 0;
        h[21] = 64;
        h[22] = 32;
        h[23] = 32;
        h[24..28].copy_from_slice(&7u32.to_be_bytes()); // change counter
        h[28..32].copy_from_slice(&pages.to_be_bytes());
        h[32..36].copy_from_slice(&trunk.to_be_bytes());
        h[36..40].copy_from_slice(&freelist.to_be_bytes());
        h[44..48].copy_from_slice(&4u32.to_be_bytes()); // schema format
        h[56..60].copy_from_slice(&1u32.to_be_bytes()); // UTF-8
        let vvf: u32 = if valid_size { 7 } else { 6 };
        h[92..96].copy_from_slice(&vvf.to_be_bytes());
        h[96..100].copy_from_slice(&3_050_004u32.to_be_bytes());

        let mut v = h;
        v.push(0x0D); // page 1 is a leaf table b-tree
        v.resize((page_size * pages) as usize, 0x00);
        v
    }

    #[test]
    fn accepts_a_well_formed_database_and_reports_its_declared_length() {
        let f = db(4096, 6, 1, 3, true);
        let out = validate(&f);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, 4096 * 6);
        assert_eq!(out.evidence_of("page_size"), Some("4096"));
        assert_eq!(out.evidence_of("page_count"), Some("6"));
        assert_eq!(out.evidence_of("freelist_pages"), Some("1"));
    }

    #[test]
    fn trailing_disk_content_does_not_extend_the_length() {
        let mut f = db(4096, 4, 0, 0, true);
        let real = f.len();
        f.extend_from_slice(&[0xAB; 8192]);
        assert_eq!(validate(&f).length, real as u64);
    }

    /// The page-size check the review asked for.
    #[test]
    fn rejects_page_sizes_that_are_not_powers_of_two_in_range() {
        for bad in [3000u16, 100, 5000, 0] {
            let mut f = db(4096, 4, 0, 0, true);
            f[16..18].copy_from_slice(&bad.to_be_bytes());
            assert_eq!(
                validate(&f).status,
                Status::Rejected,
                "page size {bad} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_the_65536_page_size_encoded_as_one() {
        let f = db(65536, 2, 0, 0, true);
        let out = validate(&f);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.evidence_of("page_size"), Some("65536"));
        assert_eq!(out.length, 65536 * 2);
    }

    /// The freelist check the review asked for.
    #[test]
    fn rejects_a_freelist_larger_than_the_database() {
        let f = db(4096, 6, 9, 3, true);
        let out = validate(&f);
        assert_eq!(out.status, Status::Rejected, "{}", out.detail);
        assert!(out.detail.contains("freelist"), "{}", out.detail);
    }

    #[test]
    fn rejects_a_freelist_trunk_outside_the_database() {
        let f = db(4096, 6, 1, 99, true);
        assert_eq!(validate(&f).status, Status::Rejected);
    }

    #[test]
    fn a_freelist_head_and_count_that_disagree_is_partial() {
        // Three free pages but no trunk page to reach them from.
        let f = db(4096, 6, 3, 0, true);
        let out = validate(&f);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(out.detail.contains("disagree"), "{}", out.detail);
    }

    /// SQLite says the in-header size is only usable when these two counters
    /// agree. Trusting it regardless is how a carve silently truncates.
    #[test]
    fn an_untrustworthy_page_count_falls_back_to_whole_pages() {
        let f = db(4096, 6, 0, 0, false);
        let out = validate(&f);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert_eq!(out.evidence_of("size_in_header_valid"), Some("false"));
        assert_eq!(out.length, 4096 * 6, "should floor to whole pages");
    }

    #[test]
    fn rejects_the_fixed_payload_fraction_bytes_being_wrong() {
        for at in [21usize, 22, 23] {
            let mut f = db(4096, 4, 0, 0, true);
            f[at] = f[at].wrapping_add(1);
            assert_eq!(validate(&f).status, Status::Rejected, "byte {at}");
        }
    }

    #[test]
    fn rejects_a_page_one_that_is_not_a_btree_page() {
        let mut f = db(4096, 4, 0, 0, true);
        f[HEADER_LEN] = 0x77;
        assert_eq!(validate(&f).status, Status::Rejected);
    }

    #[test]
    fn a_database_cut_short_is_partial_and_floors_to_whole_pages() {
        let f = db(4096, 8, 0, 0, true);
        let out = validate(&f[..4096 * 3 + 100]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert_eq!(out.length, 4096 * 3);
        assert!(out.detail.contains("truncated"), "{}", out.detail);
    }

    #[test]
    fn rejects_a_nonzero_reserved_range() {
        let mut f = db(4096, 4, 0, 0, true);
        f[80] = 1;
        assert_eq!(validate(&f).status, Status::Rejected);
    }
}
