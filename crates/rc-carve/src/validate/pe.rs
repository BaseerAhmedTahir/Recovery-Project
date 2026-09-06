//! PE (Windows executable) validator: `MZ` is two bytes and means nothing.
//!
//! The DOS header is a 1980s artefact that every Windows binary still carries.
//! Its only load-bearing field for this purpose is `e_lfanew` at offset 0x3C,
//! which points at the real PE header - so the check is that the pointer lands
//! on `PE\0\0` inside the data, that the COFF header behind it is sane, and
//! that the section table describes ranges that exist.
//!
//! Like the ICO validator this was written because of a measurement: `exe`
//! contributed 50 candidates to the quick-formatted fixture, which contains no
//! executables at all.
//!
//! Length is the end of the last section's raw data, extended to cover the
//! certificate table when the binary is signed.
//!
//! That second part was not in the first version, and a real file is what
//! caught it: the independent corpus copies the running Python interpreter,
//! which is Authenticode-signed, and the validator reported 98816 bytes for a
//! 101477-byte file. The missing 2661 are the signature, which lives past the
//! last section and is therefore invisible to a section-table walk. A carve
//! that trusted that number would produce an executable Windows refuses to run.
//!
//! PE records where it is: data directory entry 4. That entry is unique in
//! holding a *file offset* rather than a relative virtual address, precisely
//! because it describes something outside the loaded image.
//!
//! An installer payload appended by a self-extracting stub is still beyond
//! reach - nothing in the headers describes it - so this can under-report
//! there, and says so rather than guessing.

use super::{le16, le32, Outcome};

const LFANEW_AT: usize = 0x3C;
const PE_MAGIC: &[u8; 4] = b"PE\0\0";
const COFF: usize = 20;
const SECTION: usize = 40;

/// A PE with more sections than this is not something a compiler produced.
const MAX_SECTIONS: u16 = 96;

pub fn validate(d: &[u8]) -> Outcome {
    if d.len() < LFANEW_AT + 4 {
        return Outcome::reject("shorter than a DOS header");
    }
    if &d[..2] != b"MZ" {
        return Outcome::reject("does not begin with MZ");
    }

    let lfanew = le32(d, LFANEW_AT).unwrap_or(0) as usize;
    // The PE header cannot sit inside the DOS header, and a real one is a few
    // hundred bytes in at most.
    if !(4..=4096).contains(&lfanew) {
        return Outcome::reject(format!("e_lfanew is {lfanew}, which is not a PE header offset"));
    }
    if d.len() < lfanew + 4 {
        return Outcome::partial(
            lfanew as u64,
            "the DOS header points past the available data; truncated before the PE header",
        )
        .with("truncated", true);
    }
    if &d[lfanew..lfanew + 4] != PE_MAGIC {
        return Outcome::reject("e_lfanew does not point at a PE signature");
    }

    let coff = lfanew + 4;
    if d.len() < coff + COFF {
        return Outcome::partial(coff as u64, "truncated inside the COFF header")
            .with("truncated", true);
    }
    let machine = le16(d, coff).unwrap_or(0);
    let sections = le16(d, coff + 2).unwrap_or(0);
    let opt_size = le16(d, coff + 16).unwrap_or(0) as usize;

    if sections == 0 || sections > MAX_SECTIONS {
        return Outcome::reject(format!("the COFF header declares {sections} sections"));
    }
    // The optional header is 224 bytes for PE32 and 240 for PE32+, plus data
    // directories; anything wildly outside that is noise.
    if opt_size > 4096 {
        return Outcome::reject(format!("the optional header is {opt_size} bytes"));
    }

    let table = coff + COFF + opt_size;
    let table_end = table + SECTION * sections as usize;
    if table_end > d.len() {
        return Outcome::partial(
            table.min(d.len()) as u64,
            "the section table extends past the available data; truncated",
        )
        .with("truncated", true);
    }

    // The certificate table, when present, sits past every section.
    let opt = coff + COFF;
    let magic = le16(d, opt).unwrap_or(0);
    let dd_base = match magic {
        0x10B => Some(opt + 96),  // PE32
        0x20B => Some(opt + 112), // PE32+
        _ => None,
    };
    let mut signature_end = 0u64;
    if let Some(dd) = dd_base {
        let rva_count = le32(d, dd - 4).unwrap_or(0);
        // Entry 4 is IMAGE_DIRECTORY_ENTRY_SECURITY.
        if rva_count > 4 {
            let entry = dd + 4 * 8;
            let cert_off = le32(d, entry).unwrap_or(0) as u64;
            let cert_size = le32(d, entry + 4).unwrap_or(0) as u64;
            if cert_off > 0 && cert_size > 0 {
                signature_end = cert_off.saturating_add(cert_size);
            }
        }
    }

    let mut end = table_end as u64;
    for i in 0..sections as usize {
        let at = table + i * SECTION;
        let raw_size = le32(d, at + 16).unwrap_or(0) as u64;
        let raw_ptr = le32(d, at + 20).unwrap_or(0) as u64;
        // A .bss-style section has no file data at all, which is legal.
        if raw_size == 0 || raw_ptr == 0 {
            continue;
        }
        if raw_ptr < table_end as u64 {
            return Outcome::reject("a section's raw data starts inside the headers");
        }
        let Some(section_end) = raw_ptr.checked_add(raw_size) else {
            return Outcome::reject("a section's pointer and size overflow");
        };
        end = end.max(section_end);
    }
    let signed = signature_end > 0;
    end = end.max(signature_end);

    let evidence = |o: Outcome| {
        o.with("machine", format!("{machine:#06X}"))
            .with("sections", sections)
            .with("pe_offset", lfanew)
            .with("signed", signed)
    };

    if end > d.len() as u64 {
        return evidence(
            Outcome::partial(
                table_end as u64,
                format!(
                    "the section table is intact but section data runs to {end}, past the \
                     {} bytes available; length runs to the end of the headers",
                    d.len()
                ),
            )
            .with("truncated", true),
        );
    }

    evidence(Outcome::valid(end).note(if signed {
        "length runs to the end of the Authenticode certificate table, which sits \
         past the last section"
    } else {
        "length is the end of the last section's raw data; an appended installer \
         payload, which no header describes, would lie beyond it"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::Status;

    fn pe(sections: u16) -> Vec<u8> {
        let lfanew = 0x80usize;
        let opt_size = 224usize;
        let table = lfanew + 4 + COFF + opt_size;
        let data_at = table + SECTION * sections as usize;

        let mut v = vec![0u8; data_at];
        v[0] = b'M';
        v[1] = b'Z';
        v[LFANEW_AT..LFANEW_AT + 4].copy_from_slice(&(lfanew as u32).to_le_bytes());
        v[lfanew..lfanew + 4].copy_from_slice(PE_MAGIC);
        let coff = lfanew + 4;
        v[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes()); // x86-64
        v[coff + 2..coff + 4].copy_from_slice(&sections.to_le_bytes());
        v[coff + 16..coff + 18].copy_from_slice(&(opt_size as u16).to_le_bytes());

        let mut offset = data_at as u32;
        for i in 0..sections as usize {
            let at = table + i * SECTION;
            let size = 512u32;
            v[at + 16..at + 20].copy_from_slice(&size.to_le_bytes());
            v[at + 20..at + 24].copy_from_slice(&offset.to_le_bytes());
            offset += size;
        }
        v.resize(offset as usize, 0x90);
        v
    }

    #[test]
    fn accepts_a_well_formed_pe() {
        let f = pe(3);
        let out = validate(&f);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, f.len() as u64);
        assert_eq!(out.evidence_of("sections"), Some("3"));
        // {:#X} writes a lowercase 0x prefix with uppercase digits.
        assert_eq!(out.evidence_of("machine"), Some("0x8664"));
    }

    #[test]
    fn trailing_bytes_do_not_extend_the_length() {
        let mut f = pe(2);
        let real = f.len();
        f.extend_from_slice(&[0xCC; 4096]);
        assert_eq!(validate(&f).length, real as u64);
    }

    /// The reason this validator exists: MZ is two bytes and the fixture has
    /// no executables in it at all.
    #[test]
    fn rejects_chance_mz_pairs() {
        let mut state = 0x0BAD_C0DEu32;
        let mut rejected = 0;
        let trials = 2000;
        for _ in 0..trials {
            let mut v = b"MZ".to_vec();
            for _ in 0..512 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                v.push(state as u8);
            }
            if validate(&v).status == Status::Rejected {
                rejected += 1;
            }
        }
        assert_eq!(rejected, trials, "every random MZ must be rejected");
    }

    #[test]
    fn rejects_mz_followed_by_zeros() {
        let mut v = b"MZ".to_vec();
        v.resize(4096, 0);
        assert_eq!(validate(&v).status, Status::Rejected);
    }

    #[test]
    fn rejects_a_pointer_that_does_not_land_on_pe() {
        let mut f = pe(1);
        f[LFANEW_AT..LFANEW_AT + 4].copy_from_slice(&0x40u32.to_le_bytes());
        assert_eq!(validate(&f).status, Status::Rejected);
    }

    /// A signed binary's certificate table lives past every section, so a
    /// section-table walk alone under-reports the file by the size of the
    /// signature. Found by the independent corpus on a real, signed
    /// interpreter: 98816 reported for a 101477-byte file.
    #[test]
    fn a_signed_binary_includes_its_certificate_table() {
        let mut f = pe(2);
        let lfanew = 0x80usize;
        let opt = lfanew + 4 + COFF;
        f[opt..opt + 2].copy_from_slice(&0x10Bu16.to_le_bytes()); // PE32
        f[opt + 92..opt + 96].copy_from_slice(&16u32.to_le_bytes()); // 16 directories

        let sections_end = f.len() as u32;
        let cert_size = 2661u32;
        let entry = opt + 96 + 4 * 8;
        f[entry..entry + 4].copy_from_slice(&sections_end.to_le_bytes());
        f[entry + 4..entry + 8].copy_from_slice(&cert_size.to_le_bytes());
        f.resize((sections_end + cert_size) as usize, 0x00);

        let out = validate(&f);
        assert_eq!(out.status, Status::Valid, "{}", out.detail);
        assert_eq!(out.length, f.len() as u64, "the signature was not counted");
        assert_eq!(out.evidence_of("signed"), Some("true"));
    }

    #[test]
    fn an_unsigned_binary_stops_at_the_last_section() {
        let f = pe(2);
        let out = validate(&f);
        assert_eq!(out.length, f.len() as u64);
        assert_eq!(out.evidence_of("signed"), Some("false"));
    }

    #[test]
    fn a_truncated_pe_reports_the_headers_it_could_read() {
        let f = pe(4);
        let out = validate(&f[..f.len() / 2]);
        assert_eq!(out.status, Status::Partial, "{}", out.detail);
        assert!(out.length > 0 && out.length <= (f.len() / 2) as u64);
    }
}
