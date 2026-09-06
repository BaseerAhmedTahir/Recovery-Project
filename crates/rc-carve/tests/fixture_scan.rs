//! The scanner against real fixtures, graded on its own denominator.
//!
//! Validator precision is measured with file boundaries handed in. This is the
//! other measurement, and it is the one that says whether carving a real drive
//! would be usable: how many candidates come out per GB, how many survive
//! validation, and where the files we actually want sit among them.
//!
//! `quickformat.img` is the fixture Milestone 3's acceptance criterion names.
//! It is a volume whose filesystem metadata was thrown away by a quick format
//! but whose file data is still there, which is exactly the situation carving
//! exists for.
//!
//! Numbers are printed even when the test passes. A carving result that is
//! only checked against a threshold tells you nothing about whether the
//! threshold was set sensibly.

use rc_carve::scan::{scan, ScanOptions};
use rc_carve::SignatureDb;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("testdata")
        .join("fixtures")
}

struct Expected {
    /// Files that were deleted before the image was taken, by sha256.
    deleted: BTreeMap<String, (String, u64)>,
}

fn load(name: &str) -> Option<(PathBuf, Expected)> {
    let img = fixture_dir().join(format!("{name}.img"));
    let json = fixture_dir().join(format!("{name}.expected.json"));
    if !img.exists() || !json.exists() {
        return None;
    }
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).ok()?).ok()?;
    let mut deleted = BTreeMap::new();
    for (path, meta) in v["files"].as_object()? {
        if meta["state"].as_str() == Some("deleted") {
            deleted.insert(
                meta["sha256"].as_str().unwrap_or_default().to_string(),
                (path.clone(), meta["size"].as_u64().unwrap_or(0)),
            );
        }
    }
    Some((img, Expected { deleted }))
}

fn sha256(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(data))
}

/// The headline measurement.
#[test]
fn quickformat_carve_reports_its_own_denominator() {
    let Some((img, exp)) = load("quickformat") else {
        eprintln!(
            "note: quickformat fixture not built; run testdata/build_fixtures.sh.\n\
             This test measures scanner precision and cannot be substituted for."
        );
        return;
    };
    let _lock = rc_device::testutil::lock_fixtures(&fixture_dir()).ok();

    let db = SignatureDb::builtin().expect("builtin db");
    let device = rc_device::open(&img, None).expect("open fixture");
    let end = device.total_sectors() * device.sector_size().get() as u64;
    let device: Arc<dyn rc_device::ReadOnlyDevice> = Arc::from(device);

    // Two runs: raw header matches, then the same scan with validators on. The
    // difference between them is the validators' contribution, and reporting
    // one without the other would credit the scanner for their work.
    let raw = scan(
        Arc::clone(&device),
        &db,
        0,
        end,
        &ScanOptions {
            validate: false,
            suppress_contained: false,
            ..Default::default()
        },
    )
    .expect("raw scan");

    let full = scan(Arc::clone(&device), &db, 0, end, &ScanOptions::default())
        .expect("validated scan");
    let s = &full.stats;

    eprintln!("\n=== quickformat carve ===");
    eprintln!(
        "  scanned            : {:.1} MiB in {:?} ({:.0} MiB/s, {} threads, {} prefilter)",
        s.bytes_scanned as f64 / (1024.0 * 1024.0),
        s.elapsed,
        s.mib_per_sec(),
        s.threads,
        s.strategy
    );
    if let Some(w) = s.cache_warning() {
        eprintln!("  NOTE               : {w}");
    }
    eprintln!("  prefilter hits     : {}", s.prefilter_hits);
    eprintln!(
        "  header matches     : {} ({:.0} per GB scanned)   <- the scanner's raw output",
        s.header_matches,
        s.header_matches_per_gb()
    );
    eprintln!(
        "  survived validation: {} ({:.1}% of header matches)",
        s.validated,
        s.validation_survival() * 100.0
    );
    eprintln!("  rejected by a validator: {}", s.rejected);
    eprintln!(
        "  suppressed as contained: {}   (thumbnails and embedded images)",
        s.suppressed_contained
    );
    eprintln!("  window-capped      : {}", s.window_capped);
    eprintln!("  candidates emitted : {}", full.candidates.len());

    // --- what did we actually get back? -----------------------------------
    let mut by_ext: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &full.candidates {
        *by_ext.entry(c.ext.as_str()).or_default() += 1;
    }
    eprintln!("  by extension       : {by_ext:?}");

    // --- byte-exact recovery ----------------------------------------------
    //
    // Read each candidate's bytes back and hash them. This is the Milestone 3
    // criterion: byte-exact recovery of contiguous files, not "we found a
    // header there".
    let mut recovered: BTreeMap<String, u64> = BTreeMap::new();
    let mut buf = Vec::new();
    for c in &full.candidates {
        if c.length == 0 || c.length > (64 << 20) {
            continue;
        }
        buf.resize(c.length as usize, 0);
        let n = device.read_bytes_at(c.offset, &mut buf).unwrap_or(0);
        if n as u64 != c.length {
            continue;
        }
        let h = sha256(&buf);
        if exp.deleted.contains_key(&h) {
            recovered.entry(h).or_insert(c.offset);
        }
    }

    let carvable = exp
        .deleted
        .values()
        .filter(|(p, _)| {
            // Only formats the signature database can carve at all. Text and
            // raw binary have no signature and are not a carving failure.
            [".jpg", ".png", ".pdf", ".docx", ".mp4", ".sqlite"]
                .iter()
                .any(|e| p.ends_with(e))
        })
        .count();

    eprintln!(
        "  byte-exact recovery: {} of {} deleted files that have a signature \
         ({} deleted in total)",
        recovered.len(),
        carvable,
        exp.deleted.len()
    );
    eprintln!(
        "  precision at recall: {} candidates emitted to recover {} files = {:.1} \
         candidates examined per file recovered",
        full.candidates.len(),
        recovered.len().max(1),
        full.candidates.len() as f64 / recovered.len().max(1) as f64
    );
    eprintln!(
        "\n  raw scan for comparison: {} header matches with validators off, \
         {} emitted with them on - the validators removed {:.1}%",
        raw.stats.header_matches,
        full.candidates.len(),
        100.0
            * (1.0
                - full.candidates.len() as f64 / raw.stats.header_matches.max(1) as f64)
    );

    // --- assertions --------------------------------------------------------
    //
    // These were set after looking at the numbers above, not before. The first
    // run of this test recovered 26 of 228 and passed, because the only
    // assertion was "at least one". A threshold picked before the measurement
    // would have enshrined that.
    assert!(s.bytes_scanned > 0, "nothing was scanned");
    assert!(
        s.header_matches > 0,
        "no header matched anywhere in the image, which cannot be right"
    );

    // Milestone 3's acceptance criterion: byte-exact recovery of contiguous
    // files. Every file in this fixture is contiguous, so anything short of
    // all of them is a regression.
    assert_eq!(
        recovered.len(),
        carvable,
        "recovered {} of {carvable} carvable files byte-exactly. Every file in the          quick-formatted fixture is contiguous, so full recall is the bar.",
        recovered.len()
    );
    assert_eq!(
        raw.stats.header_matches, s.header_matches,
        "turning validators on changed the number of header matches; \
         validation must not affect what the scanner finds"
    );
    assert!(
        !recovered.is_empty(),
        "not one deleted file was recovered byte-exactly from the quick-formatted \
         fixture, which is the whole point of Milestone 3"
    );
}

/// Whatever else changes, the scan must not write to the source.
#[test]
fn carving_does_not_modify_the_fixture() {
    let Some((img, _)) = load("quickformat") else {
        return;
    };
    let _lock = rc_device::testutil::lock_fixtures(&fixture_dir()).ok();

    let before = sha256(&std::fs::read(&img).expect("read fixture"));
    {
        let db = SignatureDb::builtin().expect("builtin db");
        let device = rc_device::open(&img, None).expect("open");
        let end = device.total_sectors() * device.sector_size().get() as u64;
        let device: Arc<dyn rc_device::ReadOnlyDevice> = Arc::from(device);
        let _ = scan(device, &db, 0, end, &ScanOptions::default()).expect("scan");
    }
    let after = sha256(&std::fs::read(&img).expect("read fixture"));
    assert_eq!(before, after, "carving modified the source image");
}

/// Containment suppression is the only answer to a thumbnail inside a photo:
/// both are genuinely valid JPEGs, so no validator can reject either.
#[test]
fn contained_candidates_are_suppressed_and_counted() {
    let Some((img, _)) = load("quickformat") else {
        return;
    };
    let _lock = rc_device::testutil::lock_fixtures(&fixture_dir()).ok();

    let db = SignatureDb::builtin().expect("builtin db");
    let device = rc_device::open(&img, None).expect("open");
    let end = device.total_sectors() * device.sector_size().get() as u64;
    let device: Arc<dyn rc_device::ReadOnlyDevice> = Arc::from(device);

    let with = scan(Arc::clone(&device), &db, 0, end, &ScanOptions::default())
        .expect("scan");
    let without = scan(
        Arc::clone(&device),
        &db,
        0,
        end,
        &ScanOptions { suppress_contained: false, ..Default::default() },
    )
    .expect("scan");

    eprintln!(
        "\ncontainment suppression: {} candidates without, {} with ({} dropped)",
        without.candidates.len(),
        with.candidates.len(),
        with.stats.suppressed_contained
    );
    assert_eq!(
        without.candidates.len() as u64 - with.stats.suppressed_contained,
        with.candidates.len() as u64,
        "the suppressed count does not account for the difference"
    );
}
