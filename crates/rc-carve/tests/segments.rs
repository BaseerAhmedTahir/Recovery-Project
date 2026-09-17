//! A scan split into segments must produce exactly what one scan produces.
//!
//! Resume after a crash (rc-session, Milestone 5) scans in segments and commits
//! each one, so "kill mid-scan and resume to identical results" reduces to this:
//! wherever the boundaries fall, the candidates are the same. Headers straddling
//! a boundary, files running on past the segment they start in, and thumbnails
//! suppressed by a photo that began in an earlier segment all have to come out
//! the same.
//!
//! Segment sizes are deliberately odd - prime-ish byte counts that do not align
//! with blocks, clusters or sectors - so boundaries land mid-header.

use rc_carve::scan::{scan, scan_segment, Candidate, Carry, ScanOptions};
use rc_carve::SignatureDb;
use std::path::PathBuf;
use std::sync::Arc;

fn fixture(name: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("testdata")
        .join("fixtures")
        .join(format!("{name}.img"));
    assert!(p.exists(), "fixture {name} is not built");
    p
}

fn key(c: &Candidate) -> String {
    format!(
        "{} {} {} {} {:?} {} {} {:?}",
        c.offset,
        c.length,
        c.signature_id,
        c.ext,
        c.status,
        c.length_established,
        c.detail,
        c.evidence
    )
}

#[test]
fn segmented_scans_match_one_scan() {
    let dir = fixture("quickformat").parent().unwrap().to_path_buf();
    let _lock = rc_device::testutil::lock_fixtures(&dir).ok();
    let dev: Arc<dyn rc_device::ReadOnlyDevice> =
        Arc::from(rc_device::open(&fixture("quickformat"), None).expect("open"));
    let end = dev.total_bytes();
    let db = SignatureDb::builtin().expect("db");
    let opts = ScanOptions::default();

    let whole: Vec<String> = scan(Arc::clone(&dev), &db, 0, end, &opts)
        .expect("scan")
        .candidates
        .iter()
        .map(key)
        .collect();
    assert!(whole.len() > 200, "the fixture should yield its corpus");

    for seg in [7_654_321u64, 33_333_331, 128 << 20] {
        let mut carry = Carry::default();
        let mut got = Vec::new();
        let mut at = 0;
        let mut n = 0;
        while at < end {
            let stop = (at + seg).min(end);
            let r = scan_segment(Arc::clone(&dev), &db, at, stop, end, &opts, &mut carry)
                .expect("segment");
            got.extend(r.candidates.iter().map(key));
            at = stop;
            n += 1;
        }
        assert_eq!(
            got.len(),
            whole.len(),
            "{n} segments of {seg} bytes found a different number of candidates"
        );
        for (i, (a, b)) in got.iter().zip(&whole).enumerate() {
            assert_eq!(a, b, "{n} segments of {seg} bytes differ at candidate {i}");
        }
    }
}
