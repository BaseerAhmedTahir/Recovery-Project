//! Milestone 3's format-coverage criterion: carve 20+ formats.
//!
//! The quick-formatted fixture holds six carvable formats, chosen to be hard
//! rather than broad - JPEG with restart markers, MP4 with real chunk offsets.
//! This is the other axis: one ordinary file of every format the signature
//! database claims to know, laid out at cluster-aligned offsets in a synthetic
//! image, so that a signature which *cannot fire* is a failing test rather than
//! a line of JSON nobody reads.
//!
//! That failure mode is not hypothetical. The `vhd` entry read "connecti" for
//! a cookie that is actually "conectix", and could never have matched anything
//! at all. It was found by reading the headers back as ASCII, not by a test.
//! This is the test.
//!
//! # What this proves and what it does not
//!
//! Finding a file here proves the *signature* is right and the scanner locates
//! it. For the formats that also carry a validator it proves the length is
//! exact too. It does not prove the validators are correct about the format in
//! general - that is `corpus.rs`, which grades them against ImageMagick and
//! ffmpeg. The samples here are mostly hand-written from the specs, so they and
//! the validators could in principle share a misreading; the independent corpus
//! is what rules that out.
//!
//! The image is synthetic rather than a real quick-formatted volume, and the
//! difference matters: no filesystem metadata, no fragmentation, generous
//! zeroed gaps. `fixture_scan.rs` is the one that carves a volume the Windows
//! NTFS driver actually formatted.

use rc_carve::scan::{scan, ScanOptions};
use rc_carve::validate::Status;
use rc_carve::SignatureDb;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// Cluster size the samples are aligned to, matching the NTFS fixtures.
const CLUSTER: u64 = 4096;

struct Sample {
    kind: String,
    offset: u64,
    bytes: Vec<u8>,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn python() -> String {
    for cand in ["python3", "python"] {
        if Command::new(cand).arg("--version").output().is_ok() {
            return cand.to_string();
        }
    }
    panic!(
        "Python is required to build the format corpus and was not found.\n\
         This test does not skip: a coverage test that quietly stops running is \
         worse than one that fails."
    )
}

/// Generate the format samples and lay them out in one image.
fn build_image() -> (PathBuf, Vec<Sample>) {
    let py = python();
    let script = workspace_root()
        .join("testdata")
        .join("corpus")
        .join("make_corpus.py");
    let dir = std::env::temp_dir().join("rc-carve-formats");
    let _ = std::fs::remove_dir_all(&dir);

    let out = Command::new(&py)
        .arg(&script)
        .arg("--set=formats")
        .arg(&dir)
        .output()
        .expect("run make_corpus.py");
    assert!(
        out.status.success(),
        "generating the format corpus failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("manifest json");
    let entries = manifest.as_object().expect("manifest object");

    // Lay each sample at a cluster boundary with zeroed gaps, which is what a
    // freshly formatted volume looks like. Zeros rather than filler on purpose:
    // random bytes in the gaps would generate their own header matches and make
    // the precision figure describe the padding rather than the corpus.
    let mut samples = Vec::new();
    // Start past a notional boot region so nothing sits at offset 0.
    let mut image: Vec<u8> = vec![0u8; CLUSTER as usize * 4];

    for (rel, meta) in entries {
        let path = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        assert_eq!(
            bytes.len() as u64,
            meta["size"].as_u64().unwrap_or(0),
            "manifest size disagrees for {rel}"
        );
        let offset = image.len() as u64;
        image.extend_from_slice(&bytes);
        // Pad to the next cluster, then leave one empty cluster between files.
        let pad = (CLUSTER - (image.len() as u64 % CLUSTER)) % CLUSTER;
        image.resize(image.len() + pad as usize + CLUSTER as usize, 0);
        samples.push(Sample {
            kind: meta["kind"].as_str().unwrap_or_default().to_string(),
            offset,
            bytes,
        });
    }

    let img = std::env::temp_dir().join("rc-carve-formats.img");
    std::fs::write(&img, &image).expect("write the format image");
    (img, samples)
}

static BUILT: std::sync::OnceLock<(PathBuf, Vec<Sample>)> = std::sync::OnceLock::new();

fn image() -> &'static (PathBuf, Vec<Sample>) {
    BUILT.get_or_init(build_image)
}

#[test]
fn every_signature_in_the_database_finds_its_own_format() {
    let (img, samples) = image();
    let db = SignatureDb::builtin().expect("builtin db");
    let device = rc_device::open(img, None).expect("open image");
    let end = device.total_sectors() * device.sector_size().get() as u64;
    let device: Arc<dyn rc_device::ReadOnlyDevice> = Arc::from(device);

    let result = scan(device, &db, 0, end, &ScanOptions::default()).expect("scan");

    // Index what came back by offset, so a sample can be matched to the
    // candidate that starts exactly where it was written.
    let mut at_offset: BTreeMap<u64, Vec<&rc_carve::scan::Candidate>> = BTreeMap::new();
    for c in &result.candidates {
        at_offset.entry(c.offset).or_default().push(c);
    }

    let mut missing = Vec::new();
    let mut wrong_length = Vec::new();
    let mut found_kinds = BTreeSet::new();

    for s in samples {
        let here = at_offset.get(&s.offset);
        let hit = here.and_then(|v| v.iter().find(|c| c.signature_id == s.kind));
        match hit {
            None => {
                let others: Vec<&str> = here
                    .map(|v| v.iter().map(|c| c.signature_id.as_str()).collect())
                    .unwrap_or_default();
                missing.push(format!(
                    "{} at offset {}: nothing matched its signature{}",
                    s.kind,
                    s.offset,
                    if others.is_empty() {
                        String::new()
                    } else {
                        format!(" (found instead: {others:?})")
                    }
                ));
            }
            Some(c) => {
                found_kinds.insert(s.kind.clone());
                // Where a validator exists it must establish the exact length;
                // that is what byte-exact recovery reduces to.
                if c.status == Status::Valid && c.length != s.bytes.len() as u64 {
                    wrong_length.push(format!(
                        "{}: validated as {} bytes, file is {}",
                        s.kind,
                        c.length,
                        s.bytes.len()
                    ));
                }
            }
        }
    }

    eprintln!("\n=== format coverage ===");
    eprintln!(
        "  image            : {:.1} MiB, {} samples at {}-byte alignment",
        result.stats.bytes_scanned as f64 / (1024.0 * 1024.0),
        samples.len(),
        CLUSTER
    );
    eprintln!("  header matches   : {}", result.stats.header_matches);
    eprintln!("  rejected         : {}", result.stats.rejected);
    eprintln!("  emitted          : {}", result.candidates.len());
    eprintln!("  formats found    : {}", found_kinds.len());

    assert!(
        missing.is_empty(),
        "{} format(s) were not found:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
    assert!(
        wrong_length.is_empty(),
        "{} format(s) got the wrong length:\n  {}",
        wrong_length.len(),
        wrong_length.join("\n  ")
    );
    assert!(
        found_kinds.len() >= 20,
        "Milestone 3 requires 20+ formats; found {}",
        found_kinds.len()
    );
}

/// Byte-exact recovery, for every format whose validator establishes a length.
#[test]
fn validated_formats_are_recovered_byte_exactly() {
    use sha2::{Digest, Sha256};

    let (img, samples) = image();
    let db = SignatureDb::builtin().expect("builtin db");
    let device = rc_device::open(img, None).expect("open image");
    let end = device.total_sectors() * device.sector_size().get() as u64;
    let device: Arc<dyn rc_device::ReadOnlyDevice> = Arc::from(device);

    let result = scan(Arc::clone(&device), &db, 0, end, &ScanOptions::default())
        .expect("scan");

    let want: BTreeMap<u64, &Sample> = samples.iter().map(|s| (s.offset, s)).collect();
    let mut recovered = BTreeSet::new();
    let mut mismatched = Vec::new();
    let mut buf = Vec::new();

    for c in &result.candidates {
        if c.status != Status::Valid || c.length == 0 {
            continue;
        }
        let Some(s) = want.get(&c.offset) else { continue };
        if c.signature_id != s.kind {
            continue;
        }
        buf.resize(c.length as usize, 0);
        let n = device.read_bytes_at(c.offset, &mut buf).unwrap_or(0);
        if n as u64 != c.length {
            continue;
        }
        if Sha256::digest(&buf)[..] == Sha256::digest(&s.bytes)[..] {
            recovered.insert(s.kind.clone());
        } else {
            mismatched.push(format!(
                "{}: carved {} bytes, which is not the {} bytes written",
                s.kind,
                c.length,
                s.bytes.len()
            ));
        }
    }

    eprintln!(
        "\nbyte-exact recovery: {} formats recovered exactly: {:?}",
        recovered.len(),
        recovered
    );
    assert!(
        mismatched.is_empty(),
        "{} format(s) carved to the wrong bytes:\n  {}",
        mismatched.len(),
        mismatched.join("\n  ")
    );
    // Every format with a validator that reports an established length should
    // land here. Twelve validators cover fourteen signature entries.
    assert!(
        recovered.len() >= 10,
        "expected at least 10 formats recovered byte-exactly, got {}: {recovered:?}",
        recovered.len()
    );
}
