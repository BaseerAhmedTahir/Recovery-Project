//! Milestone 5 acceptance: "classification matches expectations on the
//! overwritten fixture".
//!
//! The expectations are per file and measured, not a rule: `testdata/overmap.py`
//! mapped every deleted file's clusters before anything was overwritten, then
//! re-read them after, and applied the agreed policy - GREEN nothing lost; RED
//! header or half or more lost; YELLOW otherwise. The image holds three kinds of
//! loss (overwritten and freed, allocated to a live file, and eleven targeted
//! overwrites chosen to exercise one kind of evidence each), including one case
//! that is undetectable by construction.
//!
//! Each deleted file is found through the NTFS metadata, scored, and compared.

use rc_score::{score_entry, Band, Context, Occupancy, Rules};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("testdata")
        .join("fixtures")
}

fn band(s: &str) -> Band {
    match s {
        "GREEN" => Band::Green,
        "YELLOW" => Band::Yellow,
        "RED" => Band::Red,
        other => panic!("unknown class {other}"),
    }
}

#[test]
fn classification_matches_the_overwritten_fixture() {
    let img = fixture_dir().join("overwritten.img");
    let json = fixture_dir().join("overwritten.expected.json");
    assert!(
        img.exists() && json.exists(),
        "the overwritten fixture is not built; run ./testdata/build_fixtures.sh overwritten"
    );
    let _lock = rc_device::testutil::lock_fixtures(&fixture_dir()).ok();
    let truth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).expect("read")).expect("parse");
    let files = truth["overwrite_truth"]["files"]
        .as_object()
        .expect("per-file truth; rebuild the fixture");
    let cases: BTreeMap<String, String> = truth["overwrite_damage"]["cases"]
        .as_array()
        .expect("damage cases")
        .iter()
        .map(|c| {
            (
                c["file"].as_str().unwrap().to_string(),
                c["case"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    let dev = rc_device::open(&img, None).expect("open fixture");
    let (fs, scan) = rc_fs::scan_volume(dev.as_ref(), 0).expect("scan");
    let occupancy = Occupancy::from_scan(&scan);
    let rules = Rules::builtin();
    let ctx = Context::new(dev.as_ref(), &scan, &occupancy, &rules, fs.to_string());

    let deleted: BTreeMap<String, &rc_fs::Entry> = scan
        .deleted()
        .filter_map(|e| e.path.clone().map(|p| (p, e)))
        .collect();

    let mut confusion: BTreeMap<(Band, Band), usize> = BTreeMap::new();
    let mut rows = Vec::new();
    let mut missing = Vec::new();
    let mut live_checked = 0usize;
    for (path, t) in files {
        let want = band(t["class"].as_str().unwrap());
        let Some(entry) = deleted.get(path) else {
            missing.push(path.clone());
            continue;
        };
        let s = score_entry(&ctx, entry).expect("score");
        *confusion.entry((want, s.band)).or_default() += 1;

        // The live-file evidence on its own terms: clusters now allocated to a
        // live file must be counted from the filesystem's allocation, exactly,
        // whatever else the content happens to show.
        let truth_live = t["lost_to_live_file"].as_u64().unwrap();
        if truth_live > 0 {
            assert_eq!(
                s.inputs.lost_to_live_file, truth_live,
                "{path}: clusters lost to the live file"
            );
            live_checked += 1;
        }
        if s.band != want {
            rows.push((path.clone(), want, s));
        }
    }

    eprintln!(
        "\n=== overwritten fixture: {} deleted files with truth, {} scored ===",
        files.len(),
        files.len() - missing.len()
    );
    eprintln!("  truth \\ scored     GREEN  YELLOW     RED");
    for want in [Band::Green, Band::Yellow, Band::Red] {
        eprintln!(
            "  {:<16} {:>7} {:>7} {:>7}",
            want.to_string(),
            confusion.get(&(want, Band::Green)).unwrap_or(&0),
            confusion.get(&(want, Band::Yellow)).unwrap_or(&0),
            confusion.get(&(want, Band::Red)).unwrap_or(&0)
        );
    }
    for (path, want, s) in &rows {
        let t = &files[path];
        eprintln!(
            "  MISMATCH {path}: truth {want} ({}; lost {}/{}), scored {} {} - {}",
            cases
                .get(path)
                .cloned()
                .unwrap_or_else(|| t["damage"].to_string()),
            t["lost_clusters"],
            t["clusters"],
            s.band,
            s.value,
            s.reasons
                .iter()
                .map(|r| format!("{}: {}", r.rule, r.detail))
                .collect::<Vec<_>>()
                .join(" | ")
        );
        eprintln!("           inputs: {:?}", s.inputs);
    }
    if !missing.is_empty() {
        eprintln!("  not found as deleted entries: {missing:?}");
    }

    // Two files cannot be found through metadata at all: the two files written
    // after the deletions - overwrite.bin, then live_after.bin - reused the
    // lowest free MFT records, which were theirs. Realistic, and not a scoring
    // failure: there is no entry to score. No targeted case may be among them,
    // or it would silently go ungraded.
    assert_eq!(
        missing.len(),
        2,
        "deleted files not found through NTFS metadata: {missing:?}"
    );
    for m in &missing {
        assert!(
            !cases.contains_key(m),
            "{m}: a targeted damage case whose metadata was reused is never graded"
        );
    }
    assert!(
        live_checked >= 1,
        "no scored file lost clusters to the live file, so allocation evidence is untested"
    );

    // No false alarm: every file that lost nothing rates GREEN.
    let false_alarms: Vec<&String> = rows
        .iter()
        .filter(|(_, want, _)| *want == Band::Green)
        .map(|(p, _, _)| p)
        .collect();
    assert!(
        false_alarms.is_empty(),
        "intact files rated below GREEN: {false_alarms:?}"
    );

    // The misclassifications, asserted exactly by damage case so that a new one
    // fails and so does a fixed one nobody wrote down. Each is a limitation
    // recorded in docs/LIMITATIONS.md 3.9:
    //
    //   one-cluster-random-binary-no-evidence  undetectable by construction:
    //       random bytes over a format-less blob leave nothing to see
    //   one-cluster-random-mp4, half-random-mp4  the basic corpus's MP4s have
    //       random samples and no avcC, so nothing in their media is checkable;
    //       tests/real_media.rs shows the same damage caught in a real H.264 file
    let mut got: Vec<String> = rows
        .iter()
        .map(|(p, _, _)| cases.get(p).cloned().unwrap_or_else(|| p.clone()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            "half-random-mp4",
            "one-cluster-random-binary-no-evidence",
            "one-cluster-random-mp4",
        ],
        "the set of misclassified files changed; if this is an improvement, update \
         docs/LIMITATIONS.md 3.9 and this list rather than deleting the check"
    );
}
