//! Recovered files actually written out, through the real `rc` binary, and
//! checked by hash against the fixtures' manifests.
//!
//! - `rc restore` on ext4 (every deleted file journal-only) and FAT32 (the
//!   fragmented file can only be assumed contiguous, and must not be rated
//!   GREEN);
//! - `rc extract --reassemble` after `rc carve` on the fragmented fixture: the
//!   same four files Milestone 4 reassembles, and nothing written wrongly;
//! - a destination that is the image being read is refused;
//! - the images are unchanged.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/fixtures")
        .join(name);
    assert!(p.exists(), "fixture {name} is not built");
    p
}

fn scratch(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("restore-{name}"));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn sha(p: &Path) -> String {
    hex::encode(Sha256::digest(std::fs::read(p).unwrap()))
}

fn rc(args: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_rc"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "rc {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn manifest(dir: &Path) -> Vec<Value> {
    serde_json::from_slice(&std::fs::read(dir.join("restore-manifest.json")).unwrap()).unwrap()
}

fn truth(img: &str) -> BTreeMap<String, Value> {
    let v: Value =
        serde_json::from_slice(&std::fs::read(fixture(&format!("{img}.expected.json"))).unwrap())
            .unwrap();
    v["files"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[test]
fn deleted_files_are_restored_byte_exact() {
    let _lock = rc_device::testutil::lock_fixtures(&fixture("")).ok();
    for (img, exact_want) in [("ext4-basic", 111), ("fat32-basic", 110)] {
        let image = fixture(&format!("{img}.img"));
        let before = sha(&image);
        let out = scratch(img);
        rc(&[
            "restore",
            image.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ]);

        let t = truth(img);
        let written = manifest(&out);
        let by_source: BTreeMap<&str, &Value> = written
            .iter()
            .map(|r| (r["source"].as_str().unwrap(), r))
            .collect();
        let mut exact = 0;
        for (path, meta) in t.iter().filter(|(_, m)| m["state"] == "deleted") {
            let r = by_source
                .get(path.as_str())
                .unwrap_or_else(|| panic!("{img}: {path} was not restored"));
            let on_disk = sha(Path::new(r["dest"].as_str().unwrap()));
            assert_eq!(
                on_disk, r["sha256"],
                "{img}: manifest hash is not the file's"
            );
            if r["sha256"] == meta["sha256"] {
                exact += 1;
            } else {
                // A wrong file must never be rated GREEN.
                let rating = r["notes"][0].as_str().unwrap();
                assert!(
                    !rating.starts_with("GREEN"),
                    "{img}: {path} wrong but {rating}"
                );
                assert_eq!(r["layout"], "assumed-contiguous", "{img}: {path}");
            }
        }
        assert_eq!(exact, exact_want, "{img}: byte-exact restores");
        assert_eq!(before, sha(&image), "{img}: restoring modified the image");
    }
}

#[test]
fn restoring_onto_the_image_being_read_is_refused() {
    let image = fixture("ext4-basic.img");
    let out = Command::new(env!("CARGO_BIN_EXE_rc"))
        .args([
            "restore",
            image.to_str().unwrap(),
            "--out",
            image.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("refusing to write"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn carved_fragments_are_reassembled_and_written() {
    let _lock = rc_device::testutil::lock_fixtures(&fixture("")).ok();
    let image = fixture("fragmented-jpeg.img");
    let dir = scratch("frag");
    std::fs::create_dir_all(&dir).unwrap();
    let index = dir.join("frag.rcindex");
    let out = dir.join("out");
    rc(&[
        "carve",
        image.to_str().unwrap(),
        "--index",
        index.to_str().unwrap(),
        "--show",
        "0",
    ]);
    rc(&[
        "extract",
        image.to_str().unwrap(),
        "--index",
        index.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
        "--reassemble",
    ]);
    let names: BTreeMap<String, String> = truth("fragmented-jpeg")
        .into_iter()
        .map(|(k, v)| (v["sha256"].as_str().unwrap().to_string(), k))
        .collect();
    let mut got = Vec::new();
    for r in manifest(&out) {
        assert_eq!(sha(Path::new(r["dest"].as_str().unwrap())), r["sha256"]);
        if r["layout"] == "reassembled" {
            let name = names
                .get(r["sha256"].as_str().unwrap())
                .unwrap_or_else(|| panic!("a reassembled file matches nothing: {r}"));
            got.push(name.clone());
        }
    }
    got.sort();
    assert_eq!(
        got,
        vec![
            "frag/large_a.jpg",
            "frag/large_b.jpg",
            "frag/large_c.mp4",
            "frag/large_f.jpg"
        ]
    );
}
