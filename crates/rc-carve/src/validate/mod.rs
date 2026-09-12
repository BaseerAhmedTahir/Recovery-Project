//! Validators: real structural decoding, not header matching.
//!
//! A signature match is a hypothesis. `FF D8 FF` shows up by chance about once
//! every 16 MB of random data, `BM` about once every 64 KB, and `RIFF` is
//! shared by WAV, AVI and WebP. If the carver emitted a candidate for every
//! header it found, recall would look excellent and the output would be
//! unusable.
//!
//! So each validator here walks the format's own internal accounting - chunk
//! lengths, box trees, checksums, page counts - and answers two questions the
//! signature cannot:
//!
//! 1. **Is this really that format?** A ceiling on false positives.
//! 2. **How long is it?** Footerless formats have no other way to end, and
//!    even footered ones need the *right* footer: a PDF may carry several
//!    `%%EOF` markers from incremental updates, and taking the first one
//!    truncates the file.
//!
//! Validators are pure functions over a byte slice. They never read the device
//! themselves, never allocate proportional to the candidate size, and never
//! panic on malformed input - every one of them is parsing bytes recovered
//! from a damaged disk, which is the definition of hostile input.

pub mod archive;
pub mod bmp;
pub mod gif;
pub mod ico;
pub mod jpeg;
mod jpeg_entropy;
pub mod mp4;
pub mod pdf;
pub mod pe;
pub mod png;
pub mod riff;
pub mod sqlite;
pub mod structured;
pub mod zip;

/// How much of the format's own structure held up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Structurally complete: the format's internal accounting is consistent
    /// from the first byte to the last, and any checksums it carries pass.
    Valid,
    /// Unmistakably this format, but truncated or damaged. Worth recovering
    /// and worth flagging - this is the YELLOW band of SPEC.md section 5.7.
    Partial,
    /// Not this format. The header matched by chance.
    Rejected,
}

impl Status {
    /// Should this candidate be emitted at all?
    pub fn is_accepted(self) -> bool {
        matches!(self, Status::Valid | Status::Partial)
    }
}

/// What a validator concluded.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub status: Status,
    /// Bytes from the candidate start that belong to this file. Zero when
    /// rejected. For `Partial` this is what could be established, which may be
    /// short of the true original length.
    pub length: u64,
    /// Why. Always populated for `Partial` and `Rejected`; validators may also
    /// leave a note on a `Valid` result.
    pub detail: String,
    /// A more precise extension than the signature could give.
    ///
    /// Every OOXML document and every JAR is a ZIP. Without this, a carve of a
    /// document folder reports `.zip` for all of it.
    pub refined_ext: Option<&'static str>,
    /// Structural facts worth keeping, so `rc-score` can explain a rating
    /// later without re-parsing the file. SPEC.md section 5.7 asks for the
    /// input vector to be inspectable rather than baked into a number.
    pub evidence: Vec<(&'static str, String)>,
    /// Whether `length` is the file's real size or merely a floor.
    ///
    /// A complete file establishes its own length. A damaged one usually does
    /// not, and then `length` is the furthest point the structure justifies -
    /// useful, but not an extent to judge other candidates against. The
    /// exception is a format that states its size in a header it still has:
    /// a truncated BMP knows exactly how big it was meant to be.
    ///
    /// The scanner only lets an established length suppress overlapping
    /// candidates. Getting this wrong cost 202 of 228 files on the
    /// quick-formatted fixture, so it is a field rather than a convention.
    pub length_established: bool,
    /// Whether the validator stopped because the data ended rather than
    /// because of anything in it.
    ///
    /// `true` means the structure held up as far as the bytes went, and more
    /// of them could extend `length` or complete the file. `false` on anything
    /// short of `Valid` means the validator found something that cannot
    /// belong - a checksum that fails, a restart marker out of sequence, a
    /// sample that does not divide into NAL units - and more bytes would not
    /// change that. Independent of `status`: a PNG cut before its first IDAT
    /// is rejected, but only because the data ran out.
    ///
    /// The scanner has no use for the difference; either way the candidate is
    /// damaged. Fragment reassembly is built on it. Appending the right
    /// cluster to a truncated file leaves it truncated, further along;
    /// appending a wrong one produces a contradiction. Until this was a field,
    /// the reassembler could only ask whether `length` grew - and an H.264
    /// sample whose tail ran 81 bytes into a cluster of filler made it grow.
    pub ran_out: bool,
}

impl Outcome {
    pub fn valid(length: u64) -> Outcome {
        Outcome {
            status: Status::Valid,
            length,
            detail: String::new(),
            refined_ext: None,
            evidence: Vec::new(),
            // A structurally complete file walked from its first byte to its
            // last; that length is the file's own.
            length_established: true,
            ran_out: false,
        }
    }

    pub fn partial(length: u64, detail: impl Into<String>) -> Outcome {
        Outcome {
            status: Status::Partial,
            length,
            detail: detail.into(),
            refined_ext: None,
            evidence: Vec::new(),
            // A floor unless the validator says otherwise via `established`.
            length_established: false,
            // Something was wrong unless the validator says via `truncated`
            // that the data merely ended. The safe default for reassembly: a
            // validator that does not say is never trusted to be extendable.
            ran_out: false,
        }
    }

    pub fn reject(detail: impl Into<String>) -> Outcome {
        Outcome {
            status: Status::Rejected,
            length: 0,
            detail: detail.into(),
            refined_ext: None,
            evidence: Vec::new(),
            length_established: false,
            ran_out: false,
        }
    }

    /// Mark this outcome as stopped by the end of the data rather than by
    /// anything in it. See [`Outcome::ran_out`]. Also recorded as evidence,
    /// so the reason survives into scoring.
    pub fn truncated(mut self) -> Outcome {
        self.ran_out = true;
        self.evidence.push(("truncated", "true".to_string()));
        self
    }

    /// Mark a `Partial` length as the file's real size, known from a header
    /// that survived even though the data did not.
    pub fn established(mut self) -> Outcome {
        self.length_established = true;
        self
    }

    pub fn with_ext(mut self, ext: &'static str) -> Outcome {
        self.refined_ext = Some(ext);
        self
    }

    pub fn note(mut self, detail: impl Into<String>) -> Outcome {
        self.detail = detail.into();
        self
    }

    pub fn with(mut self, key: &'static str, value: impl std::fmt::Display) -> Outcome {
        self.evidence.push((key, value.to_string()));
        self
    }

    pub fn is_valid(&self) -> bool {
        self.status == Status::Valid
    }

    pub fn is_accepted(&self) -> bool {
        self.status.is_accepted()
    }

    /// Look up one evidence value. Test and reporting convenience.
    pub fn evidence_of(&self, key: &str) -> Option<&str> {
        self.evidence
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Every validator id this crate implements.
///
/// Kept next to the dispatcher so the two cannot drift apart, and asserted
/// against the shipped signature database in the tests below.
pub const VALIDATOR_IDS: &[&str] = &[
    "7z",
    "bmp",
    "cab",
    "elf",
    "evtx",
    "gif",
    "ico",
    "jpeg",
    "mp4",
    "pdf",
    "pe",
    "png",
    "psd",
    "rar",
    "reg_hive",
    "riff_aiff",
    "riff_avi",
    "riff_wav",
    "riff_webp",
    "rtf",
    "sqlite",
    "tiff",
    "zip",
];

/// Run the validator named by `id` over a candidate.
///
/// `data` must start at the candidate's first byte and should extend to the
/// signature's `max_size` or the end of available data, whichever is smaller.
/// Returns `None` for an unknown id, which the caller should treat as "no
/// validator" rather than as a failure - most of the 44 signatures are
/// header-match-only by design.
pub fn validate(id: &str, data: &[u8]) -> Option<Outcome> {
    Some(match id {
        "7z" => archive::validate_7z(data),
        "bmp" => bmp::validate(data),
        "cab" => archive::validate_cab(data),
        "elf" => structured::validate_elf(data),
        "evtx" => structured::validate_evtx(data),
        "gif" => gif::validate(data),
        "ico" => ico::validate(data),
        "jpeg" => jpeg::validate(data),
        "mp4" => mp4::validate(data),
        "pdf" => pdf::validate(data),
        "pe" => pe::validate(data),
        "png" => png::validate(data),
        "psd" => structured::validate_psd(data),
        "rar" => archive::validate_rar(data),
        "reg_hive" => structured::validate_reg_hive(data),
        "riff_aiff" => riff::validate_aiff(data),
        "riff_avi" => riff::validate_avi(data),
        "riff_wav" => riff::validate_wav(data),
        "riff_webp" => riff::validate_webp(data),
        "rtf" => structured::validate_rtf(data),
        "sqlite" => sqlite::validate(data),
        "tiff" => structured::validate_tiff(data),
        "zip" => zip::validate(data),
        _ => return None,
    })
}

pub fn has_validator(id: &str) -> bool {
    VALIDATOR_IDS.contains(&id)
}

/// Big-endian helpers. Every one returns `None` rather than panicking, because
/// the input is bytes off a damaged disk and a truncated read is the normal
/// case rather than the exceptional one.
pub(crate) fn be16(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

pub(crate) fn be32(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

pub(crate) fn be64(d: &[u8], at: usize) -> Option<u64> {
    d.get(at..at + 8)
        .map(|b| u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

pub(crate) fn le16(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

pub(crate) fn le32(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

pub(crate) fn le64(d: &[u8], at: usize) -> Option<u64> {
    d.get(at..at + 8)
        .map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SignatureDb;

    /// A typo in `signatures.json` would otherwise disable a validator
    /// silently: the signature would still match, the lookup would return
    /// `None`, and every candidate would be emitted unvalidated. That is a
    /// precision collapse that no test of the validators themselves catches.
    #[test]
    fn every_validator_named_by_the_shipped_database_exists() {
        let db = SignatureDb::builtin().expect("builtin database must load");
        let mut missing = Vec::new();
        for sig in &db.signatures {
            if let Some(v) = &sig.validator {
                if v != "none" && !has_validator(v) {
                    missing.push(format!("{} -> {v}", sig.id));
                }
            }
        }
        assert!(missing.is_empty(), "unimplemented validators: {missing:?}");
    }

    /// The other direction: a validator nothing references is dead code, and
    /// usually means a signature was meant to name it and does not.
    #[test]
    fn every_implemented_validator_is_referenced_by_the_database() {
        let db = SignatureDb::builtin().expect("builtin database must load");
        let used: Vec<&str> = db
            .signatures
            .iter()
            .filter_map(|s| s.validator.as_deref())
            .collect();
        let unused: Vec<&&str> = VALIDATOR_IDS
            .iter()
            .filter(|id| !used.contains(&**id))
            .collect();
        assert!(
            unused.is_empty(),
            "validators nothing references: {unused:?}"
        );
    }

    #[test]
    fn unknown_ids_are_none_rather_than_a_failure() {
        assert!(validate("no_such_validator", b"anything").is_none());
        assert!(validate("none", b"anything").is_none());
    }

    /// No validator may report a length that is really just the size of the
    /// buffer it was handed.
    ///
    /// This is the general form of a bug that destroyed recall on the
    /// quick-formatted fixture. The ZIP validator, given a truncated archive,
    /// returned `d.len()` as the length. Carving hands a validator a fixed
    /// window, so that number was the window size - 16 MiB - and a candidate
    /// carrying a length it had not earned suppressed every real file inside
    /// that span. Recall went from 228 of 228 to 26.
    ///
    /// The test: feed each validator a prefix of a real file with three
    /// different amounts of trailing padding. Whatever the verdict, the
    /// reported length must not move with the padding. A length that tracks
    /// the buffer size is a fact about the caller, not about the file.
    #[test]
    fn no_validator_reports_the_buffer_size_as_a_length() {
        // Deliberately truncated samples: each is the front of something real,
        // cut so no validator can find a proper end.
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("jpeg", {
                let mut v = vec![0xFF, 0xD8];
                v.extend_from_slice(&[
                    0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x10, 0x01, 0x01, 0x11, 0x00,
                ]);
                v.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x00, 0x00, 0x00, 0x3F, 0x00]);
                v.extend_from_slice(&[0x11; 32]);
                v
            }),
            ("png", {
                let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
                let ihdr = [0, 0, 0, 8, 0, 0, 0, 8, 8, 2, 0, 0, 0];
                v.extend_from_slice(&13u32.to_be_bytes());
                v.extend_from_slice(b"IHDR");
                v.extend_from_slice(&ihdr);
                v.extend_from_slice(&crate::crc32::crc32_parts(&[b"IHDR", &ihdr]).to_be_bytes());
                let idat = [0x78u8, 0x9C, 0x63, 0x00];
                v.extend_from_slice(&(idat.len() as u32).to_be_bytes());
                v.extend_from_slice(b"IDAT");
                v.extend_from_slice(&idat);
                v.extend_from_slice(&crate::crc32::crc32_parts(&[b"IDAT", &idat]).to_be_bytes());
                v
            }),
            (
                "pdf",
                b"%PDF-1.7
1 0 obj
<< /Type /Catalog >>
endobj
"
                .to_vec(),
            ),
            ("zip", {
                let mut v = b"PK\x03\x04".to_vec();
                v.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
                v.extend_from_slice(&0u32.to_le_bytes());
                v.extend_from_slice(&5u32.to_le_bytes());
                v.extend_from_slice(&5u32.to_le_bytes());
                v.extend_from_slice(&5u16.to_le_bytes());
                v.extend_from_slice(&0u16.to_le_bytes());
                v.extend_from_slice(b"a.txt");
                v.extend_from_slice(b"hello");
                v
            }),
            ("mp4", {
                let mut v = 24u32.to_be_bytes().to_vec();
                v.extend_from_slice(b"ftypisom\x00\x00\x02\x00isom");
                v.extend_from_slice(&64u32.to_be_bytes());
                v.extend_from_slice(b"moov");
                v.extend_from_slice(&[0x11; 56]);
                v
            }),
            ("riff_wav", {
                let mut v = b"RIFF".to_vec();
                v.extend_from_slice(&0xFFFF_0000u32.to_le_bytes());
                v.extend_from_slice(b"WAVE");
                v.extend_from_slice(b"fmt ");
                v.extend_from_slice(&16u32.to_le_bytes());
                v.extend_from_slice(&[1, 0, 2, 0, 0x44, 0xAC, 0, 0, 0x10, 0xB1, 2, 0, 4, 0, 16, 0]);
                v.extend_from_slice(b"data");
                v.extend_from_slice(&0x00FF_0000u32.to_le_bytes());
                v.extend_from_slice(&[0x22; 64]);
                v
            }),
        ];

        let mut offenders = Vec::new();
        for (id, head) in &cases {
            let mut seen: Vec<(usize, u64, Status)> = Vec::new();
            for pad in [0usize, 4096, 256 * 1024] {
                let mut v = head.clone();
                v.resize(head.len() + pad, 0);
                let out = validate(id, &v).expect("validator exists");
                seen.push((pad, out.length, out.status));
            }
            let first = seen[0].1;
            if seen.iter().any(|(_, l, _)| *l != first) {
                offenders.push(format!("{id}: length varies with padding: {seen:?}"));
            }
            // And it must never exceed the real prefix, which is all the file
            // there ever was.
            if first > head.len() as u64 {
                offenders.push(format!(
                    "{id}: claimed {first} bytes from a {}-byte prefix",
                    head.len()
                ));
            }
        }
        assert!(
            offenders.is_empty(),
            "{}",
            offenders.join(
                "
"
            )
        );
    }

    /// Every validator must survive arbitrary input without panicking. These
    /// parse bytes off a damaged disk; a panic is a denial of service on the
    /// whole scan.
    #[test]
    fn no_validator_panics_on_hostile_input() {
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0u8; 1],
            vec![0u8; 3],
            vec![0xFF; 64],
            b"SQLite format 3\0".to_vec(),
            b"%PDF-".to_vec(),
            b"RIFF".to_vec(),
            b"PK\x03\x04".to_vec(),
            // Length fields claiming far more than exists.
            {
                let mut v = b"RIFF".to_vec();
                v.extend_from_slice(&u32::MAX.to_le_bytes());
                v.extend_from_slice(b"WAVE");
                v
            },
            {
                let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
                v.extend_from_slice(&u32::MAX.to_be_bytes());
                v.extend_from_slice(b"IHDR");
                v
            },
            // A box claiming a 64-bit size that would overflow a naive adder.
            {
                let mut v = 1u32.to_be_bytes().to_vec();
                v.extend_from_slice(b"ftyp");
                v.extend_from_slice(&u64::MAX.to_be_bytes());
                v
            },
        ];
        for id in VALIDATOR_IDS {
            for case in &cases {
                let out = validate(id, case).unwrap_or_else(|| panic!("{id} must be dispatchable"));
                // A rejected candidate must not claim a length.
                if out.status == Status::Rejected {
                    assert_eq!(out.length, 0, "{id} gave a length for a rejection");
                }
                // An accepted candidate must never claim more than it was given.
                assert!(
                    out.length <= case.len() as u64,
                    "{id} claimed {} bytes from a {}-byte input",
                    out.length,
                    case.len()
                );
            }
        }
    }
}
