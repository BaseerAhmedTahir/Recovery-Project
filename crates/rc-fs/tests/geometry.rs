//! A volume's geometry, checked against the one thing that cannot be argued
//! with: the files it holds.
//!
//! Scoring (Milestone 5) reads a file's content from its cluster runs and marks
//! the clusters live files occupy, and both depend on turning a cluster number
//! into a device offset. The three parsers number clusters differently - NTFS
//! from 0 at the start of the volume, FAT and exFAT from 2 at the start of a
//! data region whose offset each computes its own way - so an off-by-one in any
//! of them would read the wrong bytes for every file on that filesystem.
//!
//! So every present file on each fixture is read through its runs and the
//! geometry and hashed, and must match the manifest. The volumes were made by
//! mkfs.ntfs, mkfs.vfat, mkfs.exfat and mkfs.ext4, not by this project, so a wrong mapping
//! has nowhere to hide.
//!
//! Fails rather than skips when a fixture is missing.

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

/// Read an entry's content through its runs, or `None` when its layout is not
/// known exactly.
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

fn check(name: &str) -> (usize, usize) {
    let img = fixture_dir().join(format!("{name}.img"));
    let json = fixture_dir().join(format!("{name}.expected.json"));
    assert!(
        img.exists() && json.exists(),
        "fixture {name} is not built; run ./testdata/build_fixtures.sh {name}"
    );
    let _lock = rc_device::testutil::lock_fixtures(&fixture_dir()).ok();
    let truth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).expect("read")).expect("parse");
    assert!(
        !truth["partitioned"].as_bool().unwrap_or(false),
        "{name}: this check reads whole-device volumes"
    );
    let present: BTreeMap<String, String> = truth["files"]
        .as_object()
        .expect("files")
        .iter()
        .filter(|(_, m)| m["state"] == "present")
        .map(|(p, m)| {
            (
                p.clone(),
                m["sha256"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();

    let dev = rc_device::open(&img, None).expect("open fixture");
    let (_, scan) = rc_fs::scan_volume(dev.as_ref(), 0).expect("scan");
    let geo = scan.geometry;
    assert!(
        geo.cluster_bytes > 0,
        "{name}: the scan reported no geometry"
    );

    let mut matched = 0usize;
    let mut wrong = Vec::new();
    let mut unread = Vec::new();
    for e in scan
        .entries
        .iter()
        .filter(|e| e.state == EntryState::Allocated && e.kind == EntryKind::File)
    {
        let Some(path) = e.path.as_ref() else {
            continue;
        };
        let Some(want) = present.get(path) else {
            continue;
        };
        match read_content(dev.as_ref(), &geo, &e.location, e.size) {
            Some(bytes) => {
                if hex::encode(Sha256::digest(&bytes)) == *want {
                    matched += 1;
                } else {
                    wrong.push(format!("{path}: {:?}", e.location));
                }
            }
            None => unread.push(format!("{path}: {:?}", e.location)),
        }
    }
    assert!(
        wrong.is_empty(),
        "{name}: {} present file(s) read the wrong bytes through their runs - the geometry \
         maps clusters to the wrong offsets:\n{}",
        wrong.len(),
        wrong
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        unread.is_empty(),
        "{name}: {} present file(s) had no exact layout to read:\n{}",
        unread.len(),
        unread
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    (matched, present.len())
}

#[test]
fn geometry_reads_every_present_file_byte_exactly() {
    let mut report = Vec::new();
    for name in [
        "ntfs-basic",
        "fat32-basic",
        "exfat-basic",
        "ext4-basic",
        "ext3-indirect",
    ] {
        let (matched, present) = check(name);
        // Every present file must have been found and read; one that is never
        // matched would make this a check of fewer files than it claims.
        assert_eq!(
            matched, present,
            "{name}: read {matched} of {present} present files"
        );
        report.push(format!("{name}: {matched}/{present}"));
    }
    eprintln!("geometry verified by content: {}", report.join(", "));
}
