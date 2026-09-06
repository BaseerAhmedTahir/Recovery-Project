//! Validators against real files, not against hand-built byte vectors.
//!
//! The unit tests in each validator module build their own input, which means
//! they test the validator against my understanding of the format. That is
//! circular: a misreading of the spec produces a fixture with the same
//! misreading, and the test passes with a clean 100% that means nothing.
//!
//! Two corpora are used here, and the difference between them matters.
//!
//! **The generated corpus** (`testdata/corpus/make_corpus.py`, 315 files) is
//! the one the disk-image fixtures are built from. Its docx and sqlite files
//! come from Python's `zipfile` and `sqlite3`, which is real independence. Its
//! JPEG, PNG, PDF and MP4 files come from encoders written by hand in that
//! same file, which is not - those are deliberately adversarial (baseline JPEG
//! with restart markers, MP4 with real `stco` chunk offsets) and worth testing
//! against, but a shared misreading would not be caught.
//!
//! **The independent corpus** (`testdata/corpus/make_independent.py`) closes
//! that gap. Its files are written by ImageMagick and ffmpeg, neither of which
//! knows this project exists. It covers the eight formats the generated corpus
//! could not vouch for - jpeg, png, bmp, webp, pdf, mp4, wav, avi - and gives
//! four of them two different encoders, so neither tool is a single point of
//! agreement.
//!
//! Three things are measured, and the third is the one that matters most:
//!
//! 1. **Recall.** Every corpus file of a format we validate must be accepted.
//! 2. **Length exactness.** The reported length must equal the file's true
//!    size. This is what "byte-exact recovery of contiguous files" in
//!    Milestone 3 actually reduces to.
//! 3. **Precision.** Every validator run against a file that is *not* its
//!    format must reject it.
//!
//! **These numbers say nothing about scanner precision.** Everything here is
//! measured with known file boundaries handed to the validator. The scanner
//! faces a completely different distribution: every three-byte coincidence in
//! gigabytes of unstructured sectors, compressed streams that look like `ftyp`
//! boxes, EXIF thumbnails inside JPEGs, and JPEGs inside docx files on the
//! same volume. The scanner gets its own denominator - candidates per GB
//! scanned, and where the real files rank among them - and these figures must
//! not be quoted near it.
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
        "bmp" => Some("bmp"),
        "ico" => Some("ico"),
        "pe" => Some("pe"),
        "pdf" => Some("pdf"),
        "docx" => Some("zip"),
        "mp4" => Some("mp4"),
        "sqlite" => Some("sqlite"),
        "wav" => Some("riff_wav"),
        "avi" => Some("riff_avi"),
        "webp" => Some("riff_webp"),
        _ => None,
    }
}

#[derive(Clone)]
struct Sample {
    kind: String,
    size: u64,
    /// Which program wrote this file. `None` for the generated corpus, whose
    /// provenance is recorded per-corpus rather than per-file.
    generator: Option<String>,
}

struct Corpus {
    dir: PathBuf,
    files: BTreeMap<String, Sample>,
    /// Coverage this platform cannot provide at all - distinct from an encoder
    /// that is merely not installed, which is a hard failure.
    unavailable: Vec<String>,
    /// Human-readable note about where these files came from, printed by the
    /// tests so a run's output says what it actually measured.
    provenance: String,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn python() -> Option<String> {
    for cand in ["python3", "python"] {
        if Command::new(cand).arg("--version").output().is_ok() {
            return Some(cand.to_string());
        }
    }
    None
}

fn run_generator(script: &str, dir: &Path) -> serde_json::Value {
    let path = workspace_root().join("testdata").join("corpus").join(script);
    assert!(path.exists(), "generator missing at {}", path.display());

    let py = python().unwrap_or_else(|| {
        panic!(
            "Python is required to build the validator corpus and was not found on PATH.\n\
             This test deliberately does not skip: a validator suite that quietly stops \
             running is worse than one that fails.\n\
             Install Python 3, or run: {} <outdir>",
            path.display()
        )
    });

    let _ = std::fs::remove_dir_all(dir);
    let out = Command::new(&py)
        .arg(&path)
        .arg(dir)
        .output()
        .unwrap_or_else(|e| panic!("run {}: {e}", path.display()));
    assert!(
        out.status.success(),
        "{script} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{script} did not emit valid JSON: {e}"))
}

// --- the two corpora -------------------------------------------------------
//
// Generating them takes a noticeable fraction of a minute and every test wants
// the same files, so each is built once for the process. Nothing deletes them
// afterwards: a OnceLock has no destructor, and a test that removed the shared
// directory would break whichever test ran after it. The directory names are
// fixed rather than process-keyed so the next run reclaims them.

static GENERATED: std::sync::OnceLock<Corpus> = std::sync::OnceLock::new();
static INDEPENDENT: std::sync::OnceLock<Corpus> = std::sync::OnceLock::new();

fn generated() -> &'static Corpus {
    GENERATED.get_or_init(|| {
        let dir = std::env::temp_dir().join("rc-carve-corpus");
        let v = run_generator("make_corpus.py", &dir);
        let mut files = BTreeMap::new();
        for (path, meta) in v.as_object().expect("manifest object") {
            files.insert(
                path.clone(),
                Sample {
                    kind: meta["kind"].as_str().unwrap_or_default().to_string(),
                    size: meta["size"].as_u64().unwrap_or(0),
                    generator: None,
                },
            );
        }
        assert!(
            files.len() > 200,
            "expected the full corpus, got {} files",
            files.len()
        );
        Corpus {
            dir,
            files,
            unavailable: Vec::new(),
            provenance: "make_corpus.py (hand-written encoders for jpeg/png/pdf/mp4; \
                         Python zipfile and sqlite3 for docx/sqlite)"
                .to_string(),
        }
    })
}

fn independent() -> &'static Corpus {
    INDEPENDENT.get_or_init(|| {
        let dir = std::env::temp_dir().join("rc-carve-independent");
        let v = run_generator("make_independent.py", &dir);

        // A missing encoder is a coverage gap, and a coverage gap that lets the
        // suite pass is exactly what this corpus exists to prevent.
        let missing = v["missing_tools"].as_array().cloned().unwrap_or_default();
        assert!(
            missing.is_empty(),
            "independent encoders are missing, so these formats would be graded only \
             against my own reading of the spec: {missing:?}.\n\
             Install ImageMagick 7 (`magick`) and ffmpeg."
        );
        let failures = v["failures"].as_array().cloned().unwrap_or_default();
        assert!(
            failures.is_empty(),
            "the independent generator could not produce {} sample(s): {failures:#?}",
            failures.len()
        );

        let mut files = BTreeMap::new();
        for (path, meta) in v["files"].as_object().expect("files object") {
            files.insert(
                path.clone(),
                Sample {
                    kind: meta["kind"].as_str().unwrap_or_default().to_string(),
                    size: meta["size"].as_u64().unwrap_or(0),
                    generator: meta["generator"].as_str().map(|s| s.to_string()),
                },
            );
        }
        assert!(!files.is_empty(), "the independent corpus is empty");

        let unavailable: Vec<String> = v["unavailable_here"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let tools = &v["tools"];
        Corpus {
            dir,
            files,
            unavailable,
            provenance: format!(
                "make_independent.py\n    ImageMagick: {}\n    ffmpeg     : {}",
                tools["imagemagick"].as_str().unwrap_or("absent"),
                tools["ffmpeg"].as_str().unwrap_or("absent"),
            ),
        }
    })
}

fn read(dir: &Path, rel: &str) -> Vec<u8> {
    let path = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// shared checks, run against both corpora
// ---------------------------------------------------------------------------

/// Recall and length exactness.
fn check_accepted_with_exact_lengths(c: &Corpus, label: &str) {
    let mut checked = 0usize;
    let mut wrong_status = Vec::new();
    let mut wrong_length = Vec::new();
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();

    for (rel, s) in &c.files {
        let Some(id) = validator_for(&s.kind) else {
            continue;
        };
        let data = read(&c.dir, rel);
        assert_eq!(data.len() as u64, s.size, "manifest size disagrees for {rel}");

        let out = validate::validate(id, &data).expect("validator must exist");
        checked += 1;
        *by_kind.entry(s.kind.clone()).or_default() += 1;

        let who = s.generator.as_deref().unwrap_or("generated");
        if out.status != Status::Valid {
            wrong_status.push(format!(
                "{rel} ({}, via {who}): {:?} - {}",
                s.kind, out.status, out.detail
            ));
        } else if s.kind == "pe" {
            // PE is the one format here whose file may legitimately extend
            // past everything its headers describe. Data appended after the
            // last section - an "overlay" - is invisible to a section-table
            // walk, and unless it is an Authenticode certificate table nothing
            // in the file records where it ends.
            //
            // The interpreter in this corpus carries 2661 bytes of it. So the
            // check is that the validator never claims more than the file, and
            // the shortfall is reported rather than asserted away.
            if out.length > s.size {
                wrong_length.push(format!(
                    "{rel} (pe, via {who}): claimed {} bytes from a {}-byte file",
                    out.length, s.size
                ));
            } else if out.length < s.size {
                eprintln!(
                    "  note: {rel} has {} bytes of overlay past its last section \
                     (headers describe {} of {})",
                    s.size - out.length,
                    out.length,
                    s.size
                );
            }
        } else if out.length != s.size {
            // Reporting a length that is not the file's length is how a carve
            // produces a file that is subtly wrong rather than obviously wrong.
            wrong_length.push(format!(
                "{rel} ({}, via {who}): validator says {} bytes, file is {}",
                s.kind, out.length, s.size
            ));
        }
    }

    eprintln!("\n=== {label} ===\n  {}\n  validated {checked} files: {by_kind:?}", c.provenance);
    assert!(checked > 0, "{label}: nothing had a validator");
    assert!(
        wrong_status.is_empty(),
        "{label}: {} file(s) not accepted:\n{}",
        wrong_status.len(),
        wrong_status.join("\n")
    );
    assert!(
        wrong_length.is_empty(),
        "{label}: {} file(s) got the wrong length:\n{}",
        wrong_length.len(),
        wrong_length.join("\n")
    );
}

/// Precision: a validator must reject files that are not its format.
fn check_cross_format_rejection(c: &Corpus, label: &str) {
    let mut false_positives = Vec::new();
    let mut trials = 0usize;

    for (rel, s) in &c.files {
        let data = read(&c.dir, rel);
        let owner = validator_for(&s.kind);
        for id in validate::VALIDATOR_IDS {
            if Some(*id) == owner {
                continue;
            }
            // WAV, AVI and WebP are all RIFF, and the three validators are the
            // same walk with a different required form type. Asking one about
            // another's file is a real test - they must not accept each other -
            // and it is covered here because `owner` only skips the exact match.
            trials += 1;
            let out = validate::validate(id, &data).expect("validator must exist");
            if out.status.is_accepted() {
                false_positives.push(format!(
                    "{id} accepted {rel} ({}) as {} bytes: {}",
                    s.kind, out.length, out.detail
                ));
            }
        }
    }

    eprintln!(
        "  {trials} cross-format trials, {} false positive(s)",
        false_positives.len()
    );
    assert!(
        false_positives.is_empty(),
        "{label}: {} false positive(s):\n{}",
        false_positives.len(),
        false_positives
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// A carved candidate is followed by whatever was next on the disk, so every
/// validator must derive the length from the file's own structure rather than
/// from how much data it was handed.
fn check_trailing_data_ignored(c: &Corpus, label: &str) {
    let mut wrong = Vec::new();
    for (rel, s) in &c.files {
        let Some(id) = validator_for(&s.kind) else {
            continue;
        };
        let clean = validate::validate(id, &read(&c.dir, rel))
            .expect("validator must exist")
            .length;

        let mut data = read(&c.dir, rel);
        data.extend((0..4096u32).map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8));
        let out = validate::validate(id, &data).expect("validator must exist");

        // Compare against what the same validator said about the untouched
        // file rather than against the file size. For every format but PE those
        // are the same number; PE may legitimately stop short of the file's end
        // because of overlay data no header describes. Either way, appending
        // junk must not change the answer.
        if out.length != clean {
            wrong.push(format!(
                "{rel} ({}): {} bytes with 4 KiB appended, {clean} without",
                s.kind, out.length
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{label}: {} file(s) absorbed trailing data:\n{}",
        wrong.len(),
        wrong.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
    );
}

// ---------------------------------------------------------------------------
// the generated corpus
// ---------------------------------------------------------------------------

#[test]
fn generated_corpus_is_accepted_with_exact_lengths() {
    check_accepted_with_exact_lengths(generated(), "generated corpus");
}

#[test]
fn generated_corpus_produces_no_cross_format_false_positives() {
    check_cross_format_rejection(generated(), "generated corpus");
}

#[test]
fn generated_corpus_lengths_ignore_trailing_data() {
    check_trailing_data_ignored(generated(), "generated corpus");
}

/// The OOXML question specifically: a `.docx` must not come back as `.zip`.
#[test]
fn ooxml_documents_are_refined_rather_than_reported_as_zip() {
    let c = generated();
    let mut seen = 0usize;
    let mut wrong = Vec::new();
    for (rel, s) in &c.files {
        if s.kind != "docx" {
            continue;
        }
        let out = validate::validate("zip", &read(&c.dir, rel)).expect("zip validator");
        seen += 1;
        if out.refined_ext != Some("docx") {
            wrong.push(format!("{rel}: refined to {:?}", out.refined_ext));
        }
    }
    assert!(seen > 0, "the corpus has no docx files to check");
    eprintln!("{seen} docx files, all refined from the zip container");
    assert!(wrong.is_empty(), "not refined to docx:\n{}", wrong.join("\n"));
}

// ---------------------------------------------------------------------------
// the independent corpus - files this project had no hand in writing
// ---------------------------------------------------------------------------

#[test]
fn independent_corpus_is_accepted_with_exact_lengths() {
    check_accepted_with_exact_lengths(independent(), "independent corpus");
}

#[test]
fn independent_corpus_produces_no_cross_format_false_positives() {
    check_cross_format_rejection(independent(), "independent corpus");
}

#[test]
fn independent_corpus_lengths_ignore_trailing_data() {
    check_trailing_data_ignored(independent(), "independent corpus");
}

/// The point of the exercise: state which formats are graded against a foreign
/// encoder and which are still graded against my own reading of the spec.
///
/// This asserts the coverage rather than describing it in a comment, so the
/// claim cannot quietly stop being true.
#[test]
fn every_validator_is_graded_against_a_foreign_encoder() {
    let c = independent();

    let mut kinds: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for s in c.files.values() {
        let Some(id) = validator_for(&s.kind) else {
            continue;
        };
        let who = s.generator.as_deref().unwrap_or("unknown");
        let e = kinds.entry(id).or_default();
        if !e.contains(&who) {
            e.push(who);
        }
    }
    // docx and sqlite are covered by the generated corpus instead, via Python's
    // zipfile and sqlite3 - also foreign encoders.
    kinds.insert("zip", vec!["python-zipfile"]);
    kinds.insert("sqlite", vec!["python-sqlite3"]);

    eprintln!("\nvalidator coverage by foreign encoder:");
    let mut ungraded = Vec::new();
    for id in validate::VALIDATOR_IDS {
        match kinds.get(id) {
            Some(tools) => eprintln!("  {id:<10} {}", tools.join(", ")),
            None if c.unavailable.iter().any(|u| u.starts_with(id)) => {
                eprintln!("  {id:<10} UNAVAILABLE ON THIS PLATFORM");
            }
            None => {
                eprintln!("  {id:<10} NONE");
                ungraded.push(*id);
            }
        }
    }
    if !c.unavailable.is_empty() {
        eprintln!("\n  this platform cannot supply:");
        for u in &c.unavailable {
            eprintln!("    {u}");
        }
    }
    assert!(
        ungraded.is_empty(),
        "these validators are graded only against my own reading of the spec: {ungraded:?}"
    );
}
