//! The signature database (SPEC.md section 5.5).
//!
//! Loaded from a JSON data file rather than compiled in, so the set can be
//! extended without rebuilding. `signatures.json` ships alongside the crate and
//! is embedded as a fallback so a stripped binary still works.

use crate::error::{CarveError, Result};
use std::collections::HashMap;

/// The database shipped with the crate, embedded so the binary works standalone.
pub const BUILTIN_JSON: &str = include_str!("../signatures.json");

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Category {
    Image,
    Video,
    Audio,
    Document,
    Archive,
    Database,
    Other,
}

impl Category {
    fn parse(s: &str) -> Category {
        match s {
            "image" => Category::Image,
            "video" => Category::Video,
            "audio" => Category::Audio,
            "document" => Category::Document,
            "archive" => Category::Archive,
            "database" => Category::Database,
            _ => Category::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Category::Image => "image",
            Category::Video => "video",
            Category::Audio => "audio",
            Category::Document => "document",
            Category::Archive => "archive",
            Category::Database => "database",
            Category::Other => "other",
        }
    }
}

/// One carvable format.
#[derive(Clone, Debug)]
pub struct Signature {
    pub id: String,
    pub ext: String,
    pub category: Category,
    /// Bytes that identify the format.
    pub header: Vec<u8>,
    /// Optional mask; a zero bit means "ignore". Same length as `header`.
    pub header_mask: Option<Vec<u8>>,
    /// Where the header sits relative to the start of the file. Non-zero for
    /// container formats like MP4, whose `ftyp` follows a 4-byte box size.
    pub header_offset: usize,
    pub footer: Option<Vec<u8>>,
    /// Hard ceiling on the carved length.
    ///
    /// Mandatory, and load-time enforced. A footerless format is terminated by
    /// nothing else, so without a ceiling one spurious header would consume the
    /// rest of the volume and emit a single enormous bogus candidate.
    pub max_size: u64,
    /// Validator id, or `None` for header-match-only formats.
    pub validator: Option<String>,
    pub note: Option<String>,
}

impl Signature {
    /// Does `window` start with this signature's header?
    ///
    /// `window` must begin at the *candidate file start*, so the header offset
    /// is applied here rather than by the caller.
    pub fn matches(&self, window: &[u8]) -> bool {
        let start = self.header_offset;
        let end = start + self.header.len();
        if window.len() < end {
            return false;
        }
        let got = &window[start..end];
        match &self.header_mask {
            None => got == self.header.as_slice(),
            Some(mask) => got
                .iter()
                .zip(&self.header)
                .zip(mask)
                .all(|((g, h), m)| (g & m) == (h & m)),
        }
    }

    /// The first byte the scanner can prefilter on, and its offset.
    ///
    /// The scanner finds candidates by searching for this single byte with a
    /// SIMD-accelerated memchr and only then does the full comparison, which is
    /// what keeps the scan near device speed.
    pub fn prefilter_byte(&self) -> Option<(u8, usize)> {
        // A masked first byte cannot be prefiltered on exactly.
        if let Some(mask) = &self.header_mask {
            if mask.first().copied().unwrap_or(0) != 0xFF {
                return None;
            }
        }
        self.header.first().map(|b| (*b, self.header_offset))
    }
}

/// The loaded database.
#[derive(Clone, Debug, Default)]
pub struct SignatureDb {
    pub signatures: Vec<Signature>,
}

impl SignatureDb {
    /// Load the database embedded in the binary.
    pub fn builtin() -> Result<SignatureDb> {
        Self::from_json(BUILTIN_JSON)
    }

    pub fn from_file(path: &std::path::Path) -> Result<SignatureDb> {
        let text = std::fs::read_to_string(path).map_err(|e| CarveError::SignatureDb {
            detail: format!("reading {}: {e}", path.display()),
        })?;
        Self::from_json(&text)
    }

    pub fn from_json(text: &str) -> Result<SignatureDb> {
        let v: serde_json::Value =
            serde_json::from_str(text).map_err(|e| CarveError::SignatureDb {
                detail: format!("parsing signature database: {e}"),
            })?;

        let arr = v["signatures"]
            .as_array()
            .ok_or_else(|| CarveError::SignatureDb {
                detail: "no 'signatures' array".into(),
            })?;

        let mut out = Vec::with_capacity(arr.len());
        let mut seen: HashMap<String, usize> = HashMap::new();

        for (i, e) in arr.iter().enumerate() {
            let id = e["id"]
                .as_str()
                .ok_or_else(|| CarveError::SignatureDb {
                    detail: format!("entry {i} has no id"),
                })?
                .to_string();

            if let Some(prev) = seen.insert(id.clone(), i) {
                return Err(CarveError::SignatureDb {
                    detail: format!("duplicate signature id {id:?} at entries {prev} and {i}"),
                });
            }

            let header = parse_hex(e["header"].as_str().unwrap_or_default()).ok_or_else(|| {
                CarveError::SignatureDb {
                    detail: format!("{id}: header is not valid hex"),
                }
            })?;
            if header.is_empty() {
                return Err(CarveError::SignatureDb {
                    detail: format!("{id}: empty header"),
                });
            }

            let header_mask = match e["header_mask"].as_str() {
                Some(s) => {
                    let m = parse_hex(s).ok_or_else(|| CarveError::SignatureDb {
                        detail: format!("{id}: header_mask is not valid hex"),
                    })?;
                    if m.len() != header.len() {
                        return Err(CarveError::SignatureDb {
                            detail: format!(
                                "{id}: header_mask is {} bytes but header is {}",
                                m.len(),
                                header.len()
                            ),
                        });
                    }
                    Some(m)
                }
                None => None,
            };

            // Enforced, not merely conventional: see the field docs.
            let max_size = e["max_size"]
                .as_u64()
                .ok_or_else(|| CarveError::SignatureDb {
                    detail: format!(
                        "{id}: max_size is mandatory. A footerless format has no other \
                         terminator, so without it one spurious header would swallow the \
                         rest of the volume."
                    ),
                })?;
            if max_size == 0 {
                return Err(CarveError::SignatureDb {
                    detail: format!("{id}: max_size must be greater than zero"),
                });
            }

            let validator = match e["validator"].as_str() {
                Some("none") | None => None,
                Some(s) => Some(s.to_string()),
            };

            out.push(Signature {
                id,
                ext: e["ext"].as_str().unwrap_or("bin").to_string(),
                category: Category::parse(e["category"].as_str().unwrap_or("other")),
                header,
                header_mask,
                header_offset: e["header_off"].as_u64().unwrap_or(0) as usize,
                footer: e["footer"].as_str().and_then(parse_hex),
                max_size,
                validator,
                note: e["note"].as_str().map(|s| s.to_string()),
            });
        }

        if out.is_empty() {
            return Err(CarveError::SignatureDb {
                detail: "the signature database is empty".into(),
            });
        }

        Ok(SignatureDb { signatures: out })
    }

    pub fn get(&self, id: &str) -> Option<&Signature> {
        self.signatures.iter().find(|s| s.id == id)
    }

    pub fn len(&self) -> usize {
        self.signatures.len()
    }

    pub fn is_empty(&self) -> bool {
        self.signatures.is_empty()
    }

    /// Longest header plus offset, which bounds how much lookahead the scanner
    /// must keep across a read-buffer boundary.
    pub fn max_header_span(&self) -> usize {
        self.signatures
            .iter()
            .map(|s| s.header_offset + s.header.len())
            .max()
            .unwrap_or(0)
    }
}

fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_database_loads() {
        let db = SignatureDb::builtin().expect("builtin database must parse");
        assert!(
            db.len() >= 40,
            "SPEC.md 5.5 asks for ~40 high-value formats to start; found {}",
            db.len()
        );
    }

    /// Every footerless format is terminated only by max_size, so a missing or
    /// absurd ceiling means one bad header eats the volume.
    #[test]
    fn every_signature_has_a_sane_max_size() {
        let db = SignatureDb::builtin().unwrap();
        for s in &db.signatures {
            assert!(s.max_size > 0, "{}: max_size must be positive", s.id);
            assert!(
                s.max_size <= 256 * 1024 * 1024 * 1024,
                "{}: max_size of {} is large enough to swallow a whole disk",
                s.id,
                s.max_size
            );
            if s.footer.is_none() {
                assert!(
                    s.max_size <= 137 * 1024 * 1024 * 1024,
                    "{}: footerless formats lean entirely on max_size",
                    s.id
                );
            }
        }
    }

    #[test]
    fn max_size_is_rejected_when_missing() {
        let json = r#"{"signatures":[{"id":"x","ext":"x","header":"AABB"}]}"#;
        let err = SignatureDb::from_json(json).unwrap_err();
        assert!(
            format!("{err}").contains("max_size is mandatory"),
            "the error should explain why: {err}"
        );
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let json = r#"{"signatures":[
            {"id":"a","ext":"a","header":"AA","max_size":10},
            {"id":"a","ext":"b","header":"BB","max_size":10}]}"#;
        assert!(SignatureDb::from_json(json).is_err());
    }

    #[test]
    fn malformed_hex_is_rejected() {
        for bad in ["ZZ", "A", ""] {
            let json = format!(
                r#"{{"signatures":[{{"id":"x","ext":"x","header":"{bad}","max_size":10}}]}}"#
            );
            assert!(
                SignatureDb::from_json(&json).is_err(),
                "header {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn headers_match_at_their_declared_offset() {
        let db = SignatureDb::builtin().unwrap();
        let jpeg = db.get("jpeg").unwrap();
        assert!(jpeg.matches(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]));
        assert!(!jpeg.matches(&[0xFF, 0xD9, 0xFF]));

        // MP4's ftyp sits at offset 4, after the box size.
        let mp4 = db.get("mp4").unwrap();
        assert_eq!(mp4.header_offset, 4);
        let mut buf = vec![0u8; 16];
        buf[4..8].copy_from_slice(b"ftyp");
        assert!(mp4.matches(&buf));
        // The same bytes at offset 0 must not match.
        let mut wrong = vec![0u8; 16];
        wrong[0..4].copy_from_slice(b"ftyp");
        assert!(!mp4.matches(&wrong));
    }

    #[test]
    fn masked_headers_ignore_masked_bits() {
        let json = r#"{"signatures":[{"id":"m","ext":"m","header":"AA00",
                        "header_mask":"FF00","max_size":100}]}"#;
        let db = SignatureDb::from_json(json).unwrap();
        let s = db.get("m").unwrap();
        assert!(s.matches(&[0xAA, 0x00]));
        assert!(s.matches(&[0xAA, 0xFF]), "the second byte is masked out");
        assert!(!s.matches(&[0xAB, 0x00]));
    }

    #[test]
    fn a_mask_of_the_wrong_length_is_rejected() {
        let json = r#"{"signatures":[{"id":"m","ext":"m","header":"AABB",
                        "header_mask":"FF","max_size":100}]}"#;
        assert!(SignatureDb::from_json(json).is_err());
    }

    /// The SQLite WAL magic is 0x377f0682 or 0x377f0683 - the low bit selects
    /// the checksum endianness - so the entry carries a mask that clears it.
    /// Without the mask half of all real WAL files would be invisible, and a
    /// mask that was too wide would match neighbouring magics as well.
    #[test]
    fn the_wal_signature_matches_both_endian_magics_and_nothing_else() {
        let db = SignatureDb::builtin().expect("builtin db");
        let wal = db.get("sqlite_wal").expect("sqlite_wal entry");
        assert!(wal.header_mask.is_some(), "the entry lost its mask");

        let mut body = vec![0u8; 32];
        for (magic, want) in [
            (0x377f_0682u32, true),  // big-endian checksums; seen on this machine
            (0x377f_0683, true),     // little-endian checksums; from the spec
            (0x377f_0680, false),
            (0x377f_0684, false),
            (0x367f_0682, false),
        ] {
            body[..4].copy_from_slice(&magic.to_be_bytes());
            assert_eq!(
                wal.matches(&body),
                want,
                "magic {magic:#010x} should {} match",
                if want { "" } else { "not" }
            );
        }
    }

    /// A header that is mostly ASCII is a string someone typed, and a typo in
    /// one is invisible: the signature simply never fires. `vhd` read
    /// "connecti" for a cookie that is actually "conectix" - one 'n', a quirk
    /// of the VHD spec - and could never have matched anything.
    #[test]
    fn the_vhd_cookie_is_the_one_real_files_carry() {
        let db = SignatureDb::builtin().expect("builtin db");
        let vhd = db.get("vhd").expect("vhd entry");
        assert_eq!(
            vhd.header, b"conectix",
            "the VHD cookie has one 'n'; confirmed against real VHDs on disk"
        );
    }

    #[test]
    fn prefilter_byte_reflects_the_header_offset() {
        let db = SignatureDb::builtin().unwrap();
        assert_eq!(db.get("jpeg").unwrap().prefilter_byte(), Some((0xFF, 0)));
        assert_eq!(db.get("mp4").unwrap().prefilter_byte(), Some((b'f', 4)));
    }

    #[test]
    fn header_span_bounds_the_scanner_lookahead() {
        let db = SignatureDb::builtin().unwrap();
        let span = db.max_header_span();
        assert!(
            span >= 16,
            "the SQLite header alone is 16 bytes, got {span}"
        );
        assert!(span < 256, "an implausibly long header: {span}");
    }

    /// OOXML and JARs are ZIPs. Emitting `.zip` for all of them would be
    /// technically true and practically useless, so the ZIP entry carries a
    /// validator that sniffs the archive's contents.
    #[test]
    fn zip_defers_to_a_validator_rather_than_emitting_bare_zip() {
        let db = SignatureDb::builtin().unwrap();
        let zip = db.get("zip").unwrap();
        assert_eq!(zip.validator.as_deref(), Some("zip"));
        assert!(zip
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("Content_Types"));
    }
}
