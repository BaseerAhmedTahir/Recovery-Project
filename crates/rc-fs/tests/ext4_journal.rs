//! Milestone 6 acceptance: "recover from EXT4 fixture including journal-only
//! recoveries".
//!
//! On this fixture (mkfs.ext4 and a Linux 6.x kernel), unlink zeroes the extent
//! tree and the name leaves no trace in the live directory blocks, so every
//! deleted file here is a journal-only recovery: its name comes from a logged
//! copy of a directory block, and its extents from a logged copy of the inode
//! from before the deletion. The test checks that claim rather than assuming it,
//! requiring every deleted name to come from the journal, and then reads
//! each deleted file's content through the recovered extents and compares its
//! SHA-256 with the manifest.
//!
//! Fails rather than skips when the fixture is missing.

use rc_fs::{DataLocation, EntryKind, EntryState, Geometry};
use sha2::{Digest, Sha256};
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

fn read_content(
    dev: &dyn rc_device::ReadOnlyDevice,
    geo: &Geometry,
    loc: &DataLocation,
    size: u64,
) -> Option<Vec<u8>> {
    match loc {
        DataLocation::Resident(bytes) => Some(bytes[..(size as usize).min(bytes.len())].to_vec()),
        DataLocation::Runs(runs) => {
            let mut out = Vec::with_capacity(size as usize);
            for run in runs {
                let len = run.cluster_count * geo.cluster_bytes;
                if run.sparse {
                    out.resize(out.len() + len as usize, 0);
                    continue;
                }
                let at = geo.cluster_offset(run.start_cluster)?;
                let mut buf = vec![0u8; len as usize];
                let n = dev.read_bytes_at(at, &mut buf).ok()?;
                out.extend_from_slice(&buf[..n]);
                if out.len() as u64 >= size {
                    break;
                }
            }
            out.truncate(size as usize);
            Some(out)
        }
        _ => None,
    }
}

#[test]
fn deleted_ext4_files_are_recovered_byte_exactly_from_the_journal() {
    let img = fixture_dir().join("ext4-basic.img");
    let json = fixture_dir().join("ext4-basic.expected.json");
    assert!(
        img.exists() && json.exists(),
        "fixture ext4-basic is not built; run ./testdata/build_fixtures.sh ext4-basic"
    );
    let _lock = rc_device::testutil::lock_fixtures(&fixture_dir()).ok();
    let truth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).expect("read")).expect("parse");
    let deleted: BTreeMap<String, String> = truth["files"]
        .as_object()
        .expect("files")
        .iter()
        .filter(|(_, m)| m["state"] == "deleted")
        .map(|(p, m)| (p.clone(), m["sha256"].as_str().unwrap_or_default().into()))
        .collect();
    assert_eq!(deleted.len(), 111, "the manifest changed; re-measure");

    let dev = rc_device::open(&img, None).expect("open fixture");
    let (_, scan) = rc_fs::scan_volume(dev.as_ref(), 0).expect("scan");
    let by_path: BTreeMap<&str, &rc_fs::Entry> = scan
        .entries
        .iter()
        .filter(|e| e.state == EntryState::Deleted && e.kind == EntryKind::File)
        .filter_map(|e| e.path.as_deref().map(|p| (p, e)))
        .collect();

    let mut exact = 0usize;
    let mut wrong = Vec::new();
    let mut missing = Vec::new();
    let mut not_from_journal = Vec::new();
    for (path, want) in &deleted {
        let Some(e) = by_path.get(path.as_str()) else {
            missing.push(path.clone());
            continue;
        };
        let journal_name = e.notes.iter().any(|n| n == "name recovered from journal");
        let journal_inode = e.notes.iter().any(|n| n.contains("pre-deletion copy"));
        if !(journal_name && journal_inode) {
            not_from_journal.push(format!("{path}: {:?}", e.notes));
        }
        match read_content(dev.as_ref(), &scan.geometry, &e.location, e.size) {
            Some(b) if hex::encode(Sha256::digest(&b)) == *want => exact += 1,
            _ => wrong.push(format!("{path}: {:?}", e.location)),
        }
    }
    eprintln!(
        "ext4 deleted files: {exact}/{} byte-exact through journal-recovered extents",
        deleted.len()
    );
    assert!(missing.is_empty(), "not recovered: {missing:?}");
    assert!(
        not_from_journal.is_empty(),
        "these were expected to be journal-only recoveries:\n{}",
        not_from_journal.join("\n")
    );
    assert!(
        wrong.is_empty(),
        "{} deleted file(s) read the wrong bytes:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    assert_eq!(exact, deleted.len());
}

/// Without the journal nothing is locatable: the live inodes of every deleted
/// file have no extents. This is what makes the recoveries above journal-only
/// rather than merely journal-assisted, measured on the image itself.
#[test]
fn the_live_inodes_of_deleted_files_hold_no_extents() {
    let img = fixture_dir().join("ext4-basic.img");
    assert!(img.exists(), "fixture ext4-basic is not built");
    let _lock = rc_device::testutil::lock_fixtures(&fixture_dir()).ok();
    let dev = rc_device::open(&img, None).expect("open fixture");
    let (_, scan) = rc_fs::scan_volume(dev.as_ref(), 0).expect("scan");

    // Superblock fields needed to find an inode on disk.
    let mut sb = vec![0u8; 1024];
    dev.read_bytes_at(1024, &mut sb).unwrap();
    let u32at = |d: &[u8], o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let block = 1024u64 << u32at(&sb, 24);
    let ipg = u32at(&sb, 40) as u64;
    let inode_size = u16::from_le_bytes([sb[88], sb[89]]) as u64;
    let mut gdt = vec![0u8; 64 * 8];
    dev.read_bytes_at(block, &mut gdt).unwrap();

    let mut checked = 0;
    for e in scan
        .entries
        .iter()
        .filter(|e| e.state == EntryState::Deleted && e.kind == EntryKind::File)
    {
        let idx = e.id - 1;
        let g = (idx / ipg) as usize;
        let table = u32at(&gdt, g * 64 + 8) as u64 | ((u32at(&gdt, g * 64 + 0x28) as u64) << 32);
        let mut raw = vec![0u8; inode_size as usize];
        dev.read_bytes_at(table * block + (idx % ipg) * inode_size, &mut raw)
            .unwrap();
        let entries = u16::from_le_bytes([raw[42], raw[43]]);
        assert!(
            u32at(&raw, 20) != 0 && entries == 0,
            "{}: live inode {} still has dtime {} and {entries} extents",
            e.display_path(),
            e.id,
            u32at(&raw, 20)
        );
        checked += 1;
    }
    assert!(checked >= 111, "only {checked} deleted files checked");
}

/// Symlink targets: a short one lives inside the inode, a long one in a block.
/// Built by mke2fs -d on ext3 with block maps; the files on the same image are
/// checked byte for byte by the geometry test.
#[test]
fn symlink_targets_are_read_from_the_inode_or_a_block() {
    let img = fixture_dir().join("ext3-indirect.img");
    let json = fixture_dir().join("ext3-indirect.expected.json");
    assert!(
        img.exists() && json.exists(),
        "fixture ext3-indirect is not built; run ./testdata/build_fixtures.sh ext3-indirect"
    );
    let _lock = rc_device::testutil::lock_fixtures(&fixture_dir()).ok();
    let truth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).expect("read")).expect("parse");
    let dev = rc_device::open(&img, None).expect("open fixture");
    let (_, scan) = rc_fs::scan_volume(dev.as_ref(), 0).expect("scan");
    let links = truth["symlinks"].as_object().expect("symlinks");
    assert_eq!(links.len(), 2);
    for (name, target) in links {
        let e = scan
            .entries
            .iter()
            .find(|e| e.path.as_deref() == Some(name.as_str()))
            .unwrap_or_else(|| panic!("{name} not found"));
        let got = read_content(dev.as_ref(), &scan.geometry, &e.location, e.size)
            .unwrap_or_else(|| panic!("{name}: no content location {:?}", e.location));
        assert_eq!(
            String::from_utf8_lossy(&got),
            target.as_str().unwrap(),
            "{name}: {:?}",
            e.location
        );
    }
    assert!(matches!(
        scan.entries
            .iter()
            .find(|e| e.name == "fast-link")
            .unwrap()
            .location,
        DataLocation::Resident(_)
    ));
}
