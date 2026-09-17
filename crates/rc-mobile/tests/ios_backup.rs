//! Milestone 7 acceptance, iOS part: "parse an iOS backup and list Recently
//! Deleted assets".
//!
//! Against a SYNTHETIC backup (`testdata/mobile/make_ios_backup.py`): real
//! SQLite databases and real plists, laid out the way iOS backups are
//! documented to be, but not made by an iPhone. See LIMITATIONS.

use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;

fn build() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(|p| p.parent())
                .expect("workspace root")
                .to_path_buf();
            let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ios-backup");
            let script = root.join("testdata/mobile/make_ios_backup.py");
            for py in ["python3", "python"] {
                if let Ok(o) = Command::new(py).arg(&script).arg(&out).output() {
                    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
                    return out;
                }
            }
            panic!("Python is required to build the iOS backup fixture");
        })
        .clone()
}

fn tree_hash(dir: &std::path::Path) -> String {
    let mut entries = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().filter_map(|e| e.ok()) {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                let h = hex::encode(Sha256::digest(std::fs::read(&p).unwrap()));
                entries.push(format!("{}:{h}", p.display()));
            }
        }
    }
    entries.sort();
    hex::encode(Sha256::digest(entries.join("\n")))
}

#[test]
fn recently_deleted_assets_are_listed_and_extracted() {
    let out = build();
    let truth: Json =
        serde_json::from_str(&std::fs::read_to_string(out.join("truth.json")).unwrap()).unwrap();
    let udid = truth["udid"].as_str().unwrap();

    let found = rc_mobile::ios::find_backups(&[out.join("backup"), out.join("encrypted")]);
    assert_eq!(found.len(), 2);
    assert!(found
        .iter()
        .all(|b| b.device_name.as_deref() == Some("Test iPhone")));
    assert_eq!(found.iter().filter(|b| b.encrypted).count(), 1);

    let dir = out.join("backup").join(udid);
    let before = tree_hash(&dir);
    let backup = rc_mobile::ios::Backup::open(&dir).expect("open backup");
    assert_eq!(
        backup.camera_roll_files() as u64,
        truth["camera_roll_files"].as_u64().unwrap()
    );
    let assets = backup.recently_deleted().expect("Photos.sqlite");

    let want = truth["trashed"].as_array().unwrap();
    assert_eq!(assets.len(), want.len(), "number of trashed assets");
    for (a, w) in assets.iter().zip(want) {
        assert_eq!(a.pk, w["pk"].as_i64().unwrap());
        assert_eq!(a.filename.as_deref(), w["filename"].as_str());
        assert_eq!(a.directory.as_deref(), w["directory"].as_str());
        assert_eq!(
            a.original_filename.as_deref(),
            w["original_filename"].as_str()
        );
        assert_eq!(a.kind, w["kind"].as_i64());
        assert_eq!(a.trashed_unix, w["trashed_unix"].as_i64());
        assert_eq!(a.created_unix, w["created_unix"].as_i64());
        match w["original_sha256"].as_str() {
            Some(sha) => {
                let f = a.original.as_ref().expect("original file located");
                assert!(f.present);
                let got = hex::encode(Sha256::digest(std::fs::read(&f.stored_at).unwrap()));
                assert_eq!(got, sha, "{:?}", a.filename);
            }
            None => assert!(a.original.is_none(), "{:?} has no original", a.filename),
        }
        let derivs = w["derivatives"].as_object().unwrap();
        assert_eq!(a.derivatives.len(), derivs.len(), "{:?}", a.filename);
        for d in &a.derivatives {
            let sha = derivs[&d.relative_path].as_str().unwrap();
            let got = hex::encode(Sha256::digest(std::fs::read(&d.stored_at).unwrap()));
            assert_eq!(got, sha);
        }
    }

    let dest = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ios-extract");
    let _ = std::fs::remove_dir_all(&dest);
    let copied = rc_mobile::ios::extract(&assets, &dest).unwrap();
    let expected_files: usize = want
        .iter()
        .map(|w| {
            usize::from(w["original_sha256"].is_string())
                + w["derivatives"].as_object().unwrap().len()
        })
        .sum();
    assert_eq!(copied.len(), expected_files);
    for (src, dst) in &copied {
        assert_eq!(
            std::fs::read(&src.stored_at).unwrap(),
            std::fs::read(dst).unwrap()
        );
    }
    // A second extraction into the same place is refused, not merged.
    assert!(rc_mobile::ios::extract(&assets, &dest).is_err());

    assert_eq!(before, tree_hash(&dir), "the backup was modified");
}

#[test]
fn an_encrypted_backup_is_refused_with_an_explanation() {
    let out = build();
    let truth: Json =
        serde_json::from_str(&std::fs::read_to_string(out.join("truth.json")).unwrap()).unwrap();
    let dir = out.join("encrypted").join(truth["udid"].as_str().unwrap());
    let err = match rc_mobile::ios::Backup::open(&dir) {
        Ok(_) => panic!("an encrypted backup must not open"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("encrypted"), "{err}");
}
