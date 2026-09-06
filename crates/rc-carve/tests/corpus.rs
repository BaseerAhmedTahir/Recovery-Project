//! Validators against the real corpus, not against hand-built byte vectors.
//!
//! The unit tests in each validator module build their own input, which means
//! they test the validator against my understanding of the format. That is
//! circular: a misreading of the spec produces a fixture with the same
//! misreading, and the test passes.
//!
//! This suite runs the validators over the 315-file corpus that
//! `testdata/corpus/make_corpus.py` produces. How much independence that buys
//! varies by format, and it is worth being exact about it:
//!
//! * **docx and sqlite are genuinely independent.** They come from Python's
//!   `zipfile` and `sqlite3`, implementations neither this crate nor its
//!   author had any hand in. Agreement there is real evidence about the
//!   format rather than about my reading of it.
//! * **jpeg, png, pdf and mp4 are only half independent.** The corpus builds
//!   them with hand-written encoders in `make_corpus.py`, so a misreading of
//!   the spec could in principle live in both the encoder and the validator.
//!   They are still worth testing against - the encoder and the validator were
//!   written from opposite directions, and the encoder emits real structures
//!   like JPEG restart markers and MP4 `stco` chunk offsets - but agreement
//!   here is weaker evidence than for docx and sqlite.
//!
//! The fixture built by `scripts/make-windows-fixture.ps1` narrows this gap
//! for the two formats where it can: its sqlite files were written by a
//! different SQLite version and its docx members deflated by a different zlib
//! than the ones that built `ntfs-basic.img`. See `docs/LIMITATIONS.md`
//! section 2.7.
//!
//! Three things are measured, and the third is the one that matters most:
//!
//! 1. **Recall.** Every corpus file of a format we validate must be accepted.
//! 2. **Length exactness.** The reported length must equal the file's true
//!    size. This is what "byte-exact recovery of contiguous files" in
//!    Milestone 3 actually reduces to.
//! 3. **Precision.** Every validator run against a file that is *not* its
//!    format must reject it. A carver that accepts everything has perfect
//!    recall and no value, and the `text` and `binary` corpus files - 87 of
//!    them - are the honest test of that.
//!
//! Following the pattern of the immutability test: this builds what it needs
//! rather than skipping when it is absent, so it is never silently a no-op.

use rc_carve::validate::{self, Status};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Which validator owns each corpus `kind`.
///
/// `text` and `binary` map to nothing on purpose: they are the negative
/// controls, and every validator must reject them.
fn validator_for(kind: &str) -> Option<&'static str> {
    match kind {
        "jpeg" => Some("jpeg"),
        "png" => Some("png"),
        "pdf" => Some("pdf"),
        "docx" => Some("zip"),
        "mp4" => Some("mp4"),
        "sqlite" => Some("sqlite"),
        _ => None,
    }
}

struct Corpus {
    dir: PathBuf,
    /// relative path -> (kind, size)
    files: BTreeMap<String, (String, u64)>,
}

/// Generating 315 files takes a noticeable fraction of a minute, and every
/// test here wants the same corpus, so build it once for the process.
///
/// Nothing deletes it afterwards: a `OnceLock` has no destructor, and a test
/// that removed the shared directory would break whichever test happened to
/// run after it. The directory name is fixed rather than process-keyed so the
/// next run reclaims it instead of leaving one behind each time.
static CORPUS: std::sync::OnceLock<Corpus> = std::sync::OnceLock::new();

fn corpus() -> &'static Corpus {
    CORPUS.get_or_init(build_corpus)
}

fn python() -> Option<String> {
    for cand in ["python3", "python"] {
        if Command::new(cand).arg("--version").output().is_ok() {
            return Some(cand.to_string());
        }
    }
    None
}

fn build_corpus() -> Corpus {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let script = repo.join("testdata").join("corpus").join("make_corpus.py");
    assert!(
        script.exists(),
        "corpus generator missing at {}",
        script.display()
    );

    let py = python().unwrap_or_else(|| {
        panic!(
            "Python is required to build the validator corpus and was not found on PATH.\n\
             This test deliberately does not skip: a validator suite that quietly \
             stops running is worse than one that fails.\n\
             Install Python 3, or run: {} <outdir>",
            script.display()
        )
    });

    let dir = std::env::temp_dir().join("rc-carve-corpus");
    let _ = std::fs::remove_dir_all(&dir);

    let out = Command::new(&py)
        .arg(&script)
        .arg(&dir)
        .output()
        .expect("run the corpus generator");
    assert!(
        out.status.success(),
        "corpus generation failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The generator writes its manifest to stdout as JSON. Parse only the two
    // fields needed here rather than pulling in a JSON dependency for a test.
    let text = String::from_utf8(out.stdout).expect("manifest is utf-8");
    let files = parse_manifest(&text);
    assert!(
        files.len() > 200,
        "expected the full corpus, got {} files",
        files.len()
    );
    Corpus { dir, files }
}

/// Minimal reader for the manifest shape `{"path": {"kind": "...", "size": N, ...}}`.
fn parse_manifest(text: &str) -> BTreeMap<String, (String, u64)> {
    let mut out = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut kind: Option<String> = None;
    let mut size: Option<u64> = None;

    for line in text.lines() {
        let t = line.trim();
        // A key at two-space indent is a file path; deeper keys are its fields.
        let indent = line.len() - line.trim_start().len();
        if indent == 2 && t.ends_with('{') {
            if let Some(name) = t.split('"').nth(1) {
                current = Some(unescape(name));
                kind = None;
                size = None;
            }
        } else if indent == 4 {
            if let Some(rest) = t.strip_prefix("\"kind\":") {
                kind = rest.trim().trim_end_matches(',').split('"').nth(1).map(unescape);
            } else if let Some(rest) = t.strip_prefix("\"size\":") {
                size = rest.trim().trim_end_matches(',').parse().ok();
            }
        } else if indent == 2 && t.starts_with('}') {
            if let (Some(c), Some(k), Some(s)) = (current.take(), kind.take(), size.take()) {
                out.insert(c, (k, s));
            }
        }
    }
    out
}

/// The manifest is `ensure_ascii` JSON, so non-ASCII names arrive escaped.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                let n = u32::from_str_radix(&hex, 16).unwrap_or(0xFFFD);
                // Surrogate pair for anything outside the BMP, e.g. emoji.
                if (0xD800..0xDC00).contains(&n) {
                    let mut low = String::new();
                    for _ in 0..2 {
                        chars.next();
                    }
                    low.extend(chars.by_ref().take(4));
                    let l = u32::from_str_radix(&low, 16).unwrap_or(0xFFFD);
                    let cp = 0x10000 + ((n - 0xD800) << 10) + (l - 0xDC00);
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                } else {
                    out.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                }
            }
            Some('/') => out.push('/'),
            Some(other) => out.push(other),
            None => break,
        }
    }
    out
}

fn read(dir: &Path, rel: &str) -> Vec<u8> {
    let path = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------

/// Recall and length exactness, in one pass so the corpus is built once.
#[test]
fn validators_accept_the_corpus_and_report_exact_lengths() {
    let c = corpus();

    let mut checked = 0usize;
    let mut wrong_status = Vec::new();
    let mut wrong_length = Vec::new();
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();

    for (rel, (kind, size)) in &c.files {
        let Some(id) = validator_for(kind) else {
            continue;
        };
        let data = read(&c.dir, rel);
        assert_eq!(data.len() as u64, *size, "manifest size disagrees for {rel}");

        let out = validate::validate(id, &data).expect("validator must exist");
        checked += 1;
        *by_kind.entry(kind.clone()).or_default() += 1;

        if out.status != Status::Valid {
            wrong_status.push(format!("{rel} ({kind}): {:?} - {}", out.status, out.detail));
        } else if out.length != *size {
            // Reporting a length that is not the file's length is how a carve
            // produces a file that is subtly wrong rather than obviously wrong.
            wrong_length.push(format!(
                "{rel} ({kind}): validator says {} bytes, file is {size}",
                out.length
            ));
        }
    }

    println!("validated {checked} corpus files: {by_kind:?}");
    assert!(checked >= 200, "only {checked} files had a validator");
    assert!(
        wrong_status.is_empty(),
        "{} corpus file(s) were not accepted:\n{}",
        wrong_status.len(),
        wrong_status.join("\n")
    );
    assert!(
        wrong_length.is_empty(),
        "{} corpus file(s) got the wrong length:\n{}",
        wrong_length.len(),
        wrong_length.join("\n")
    );
}

/// Precision: a validator must reject files that are not its format.
///
/// This is the half of the measurement that recall alone hides. The corpus has
/// 87 `text` and `binary` files that are not any carvable format, and every
/// validator must turn all of them down.
#[test]
fn validators_reject_formats_that_are_not_theirs() {
    let c = corpus();

    let mut false_positives = Vec::new();
    let mut trials = 0usize;

    for (rel, (kind, _)) in &c.files {
        let data = read(&c.dir, rel);
        let owner = validator_for(kind);
        for id in validate::VALIDATOR_IDS {
            // Skip the validator that legitimately owns this file.
            if Some(*id) == owner {
                continue;
            }
            trials += 1;
            let out = validate::validate(id, &data).expect("validator must exist");
            if out.status.is_accepted() {
                false_positives.push(format!(
                    "{id} accepted {rel} ({kind}) as {} bytes: {}",
                    out.length, out.detail
                ));
            }
        }
    }

    println!(
        "{trials} cross-format trials, {} false positive(s)",
        false_positives.len()
    );
    assert!(
        false_positives.is_empty(),
        "{} false positive(s):\n{}",
        false_positives.len(),
        false_positives
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The OOXML question specifically: a `.docx` must not come back as `.zip`.
#[test]
fn ooxml_documents_are_refined_rather_than_reported_as_zip() {
    let c = corpus();

    let mut seen = 0usize;
    let mut wrong = Vec::new();
    for (rel, (kind, _)) in &c.files {
        if kind != "docx" {
            continue;
        }
        let data = read(&c.dir, rel);
        let out = validate::validate("zip", &data).expect("zip validator");
        seen += 1;
        if out.refined_ext != Some("docx") {
            wrong.push(format!("{rel}: refined to {:?}", out.refined_ext));
        }
    }

    assert!(seen > 0, "the corpus has no docx files to check");
    println!("{seen} docx files, all refined from the zip container");
    assert!(wrong.is_empty(), "not refined to docx:\n{}", wrong.join("\n"));
}

/// A carved candidate is followed by whatever was next on the disk, so every
/// validator must derive the length from the file's own structure rather than
/// from how much data it was handed.
#[test]
fn trailing_disk_content_never_extends_a_reported_length() {
    let c = corpus();

    let mut wrong = Vec::new();
    for (rel, (kind, size)) in &c.files {
        let Some(id) = validator_for(kind) else {
            continue;
        };
        let mut data = read(&c.dir, rel);
        // Whatever happened to be in the next clusters.
        data.extend((0..4096u32).map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8));

        let out = validate::validate(id, &data).expect("validator must exist");
        if out.length != *size {
            wrong.push(format!(
                "{rel} ({kind}): {} bytes reported for a {size}-byte file with 4 KiB appended",
                out.length
            ));
        }
    }

    assert!(
        wrong.is_empty(),
        "{} file(s) absorbed trailing data:\n{}",
        wrong.len(),
        wrong.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );
}
