//! The GUI's operations on real fixtures: a filesystem scan fills rows, a
//! filter finds them, restore writes them byte-exact, preview renders one.

use rc_results::{restore, scan_filesystems, Filter, Store};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

#[test]
fn scan_filter_restore_and_preview_on_ext4() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures");
    let image = root.join("ext4-basic.img");
    assert!(image.exists(), "fixture ext4-basic is not built");
    let _lock = rc_device::testutil::lock_fixtures(&root).ok();
    let truth: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("ext4-basic.expected.json")).unwrap())
            .unwrap();

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gui-ops");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut store = Store::open(&dir.join("results.sqlite")).unwrap();
    let mut phases = Vec::new();
    let found = scan_filesystems(&image, &AtomicBool::new(false), &mut |p| {
        phases.push(p.phase)
    })
    .unwrap();
    let summary = store.add(found).unwrap();
    assert!(summary.rows_added >= 111, "{summary:?}");
    assert!(!phases.is_empty());

    // The same deleted file recorded twice in the journal is listed once:
    // no two rows share a path, a size and a starting block.
    let all = store.set_view(&Filter::default()).unwrap();
    let mut seen = std::collections::HashSet::new();
    for r in store.rows(0, all).unwrap() {
        assert!(
            seen.insert((r.path.clone(), r.size, r.offset)),
            "{} is listed twice",
            r.path
        );
    }

    // The deleted corpus files, found by filter.
    let deleted: BTreeMap<String, String> = truth["files"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, m)| m["state"] == "deleted")
        .map(|(k, m)| (k.clone(), m["sha256"].as_str().unwrap().to_string()))
        .collect();
    let n = store
        .set_view(&Filter {
            bands: vec!["GREEN".into()],
            kinds: vec!["deleted".into()],
            sort: Some("path".into()),
            ..Default::default()
        })
        .unwrap();
    let rows = store.rows(0, n).unwrap();
    let wanted: Vec<i64> = rows
        .iter()
        .filter(|r| deleted.contains_key(&r.path))
        .map(|r| r.id)
        .collect();
    assert_eq!(
        wanted.len(),
        deleted.len(),
        "every deleted corpus file is a GREEN row"
    );

    let out = dir.join("restored");
    let written = restore(&store, &wanted, &out, false).unwrap();
    let exact = written
        .iter()
        .filter(|w| deleted.get(&w.source) == Some(&w.sha256))
        .count();
    assert_eq!(exact, deleted.len(), "restored byte-exact");
    for w in &written {
        assert_eq!(
            hex::encode(Sha256::digest(std::fs::read(&w.dest).unwrap())),
            w.sha256
        );
    }

    // A preview of a deleted image renders.
    let jpeg = rows
        .iter()
        .find(|r| r.ext == "jpg" && deleted.contains_key(&r.path))
        .expect("a deleted jpeg");
    let png = rc_results::preview(&store, jpeg.id, 128).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let hex = rc_results::hex(&store, jpeg.id, 0, 64).unwrap();
    assert_eq!(
        &hex.bytes[..2],
        &[0xFF, 0xD8],
        "hex view starts at the JPEG header"
    );
}
