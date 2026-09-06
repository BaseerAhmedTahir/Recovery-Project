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

pub mod bmp;
pub mod jpeg;
pub mod mp4;
pub mod pdf;
pub mod png;
pub mod riff;
pub mod sqlite;
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
}

impl Outcome {
    pub fn valid(length: u64) -> Outcome {
        Outcome {
            status: Status::Valid,
            length,
            detail: String::new(),
            refined_ext: None,
            evidence: Vec::new(),
        }
    }

    pub fn partial(length: u64, detail: impl Into<String>) -> Outcome {
        Outcome {
            status: Status::Partial,
            length,
            detail: detail.into(),
            refined_ext: None,
            evidence: Vec::new(),
        }
    }

    pub fn reject(detail: impl Into<String>) -> Outcome {
        Outcome {
            status: Status::Rejected,
            length: 0,
            detail: detail.into(),
            refined_ext: None,
            evidence: Vec::new(),
        }
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
    "bmp", "jpeg", "mp4", "pdf", "png", "riff_avi", "riff_wav", "riff_webp", "sqlite", "zip",
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
        "bmp" => bmp::validate(data),
        "jpeg" => jpeg::validate(data),
        "mp4" => mp4::validate(data),
        "pdf" => pdf::validate(data),
        "png" => png::validate(data),
        "riff_avi" => riff::validate_avi(data),
        "riff_wav" => riff::validate_wav(data),
        "riff_webp" => riff::validate_webp(data),
        "sqlite" => sqlite::validate(data),
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
    d.get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

pub(crate) fn be32(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

pub(crate) fn be64(d: &[u8], at: usize) -> Option<u64> {
    d.get(at..at + 8).map(|b| {
        u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    })
}

pub(crate) fn le16(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

pub(crate) fn le32(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

pub(crate) fn le64(d: &[u8], at: usize) -> Option<u64> {
    d.get(at..at + 8).map(|b| {
        u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
    })
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
        assert!(unused.is_empty(), "validators nothing references: {unused:?}");
    }

    #[test]
    fn unknown_ids_are_none_rather_than_a_failure() {
        assert!(validate("no_such_validator", b"anything").is_none());
        assert!(validate("none", b"anything").is_none());
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
                let out = validate(id, case)
                    .unwrap_or_else(|| panic!("{id} must be dispatchable"));
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
