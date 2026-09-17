//! Milestone 7 acceptance: "carve deleted SMS rows from a SQLite fixture".
//!
//! The databases are written by the SQLite library Python links against
//! (`testdata/sqlite/make_sms_db.py`), into a directory under the target dir,
//! with Android's `sms` schema. The generator records every message and, for
//! each deleted one, whether its body text is still anywhere in the files: the
//! measured ceiling on what carving could find.
//!
//! Checked:
//! - every recovered row is a row that was really deleted, with every column
//!   exactly as written (nothing invented, nothing live reported as deleted);
//! - every deleted row whose bytes survive is recovered, or the miss is listed;
//! - the files are unchanged afterwards and no `-shm` or journal appears.

use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Build once per test binary: the tests run in parallel and share the output.
fn build() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT.get_or_init(build_uncached).clone()
}

fn build_uncached() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf();
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sms-fixture");
    let script = root.join("testdata").join("sqlite").join("make_sms_db.py");
    for py in ["python3", "python"] {
        if let Ok(o) = Command::new(py).arg(&script).arg(&out).output() {
            assert!(
                o.status.success(),
                "make_sms_db.py failed: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return out;
        }
    }
    panic!("Python is required to build the SQLite fixture and was not found");
}

fn listing(dir: &Path) -> BTreeMap<String, String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| {
            let bytes = std::fs::read(e.path()).unwrap();
            (
                e.file_name().to_string_lossy().to_string(),
                hex::encode(Sha256::digest(&bytes)),
            )
        })
        .collect()
}

/// A row as JSON values, in schema order, for comparison with the truth.
fn as_json(values: &[rc_sqlite_carve::format::Value]) -> Vec<Json> {
    values
        .iter()
        .map(|v| serde_json::to_value(v).unwrap())
        .collect()
}

struct Outcome {
    deleted: usize,
    surviving: usize,
    recovered: usize,
    missed: Vec<String>,
}

fn check(dir: &Path, truth: &Json, name: &str) -> Outcome {
    let db = &truth["databases"][name];
    let rows = db["rows"].as_array().unwrap();
    let body_col = 12;
    let id_col = 0;

    let before = listing(dir);
    let report = rc_sqlite_carve::carve_file(&dir.join(db["file"].as_str().unwrap()))
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(
        before,
        listing(dir),
        "{name}: carving changed or created files"
    );

    let sms = report
        .tables
        .iter()
        .find(|t| t.name == "sms")
        .expect("sms table in schema");
    let live: Vec<&Json> = rows.iter().filter(|r| r["state"] == "live").collect();
    assert_eq!(sms.live_rows, live.len(), "{name}: live row count");

    // Truth by body, which is unique per message.
    let by_body: BTreeMap<&str, &Json> = rows
        .iter()
        .map(|r| (r["values"][body_col].as_str().unwrap(), r))
        .collect();

    let mut got = BTreeSet::new();
    for rec in report.recovered.iter().filter(|r| r.table == "sms") {
        let vals = as_json(&rec.values);
        let body = vals[body_col]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: recovered row without a text body: {vals:?}"));
        let t = by_body
            .get(body)
            .unwrap_or_else(|| panic!("{name}: recovered a row that was never written: {vals:?}"));
        assert_eq!(
            t["state"], "deleted",
            "{name}: a live row was reported as deleted: {body}"
        );
        let want = t["values"].as_array().unwrap();
        for (i, (w, g)) in want.iter().zip(&vals).enumerate() {
            if i == id_col && g.is_null() && rec.rowid.is_none() {
                continue; // the rowid did not survive; the alias column is NULL
            }
            assert_eq!(w, g, "{name}: column {i} of {body} ({:?})", rec.source);
        }
        assert!(
            got.insert(body.to_string()),
            "{name}: {body} reported twice"
        );
    }

    let deleted: Vec<&Json> = rows.iter().filter(|r| r["state"] == "deleted").collect();
    let surviving: Vec<&Json> = deleted
        .iter()
        .copied()
        .filter(|r| r["body_in_files"] == true)
        .collect();
    let missed: Vec<String> = surviving
        .iter()
        .map(|r| r["values"][body_col].as_str().unwrap().to_string())
        .filter(|b| !got.contains(b))
        .collect();
    Outcome {
        deleted: deleted.len(),
        surviving: surviving.len(),
        recovered: got.len(),
        missed,
    }
}

#[test]
fn deleted_sms_rows_are_carved_exactly() {
    let dir = build();
    let truth: Json =
        serde_json::from_str(&std::fs::read_to_string(dir.join("truth.json")).unwrap()).unwrap();
    eprintln!("SQLite {}", truth["sqlite_version"]);
    for name in ["rollback", "wal"] {
        let o = check(&dir, &truth, name);
        eprintln!(
            "{name}: {} deleted, {} with their body still in the files, {} recovered",
            o.deleted, o.surviving, o.recovered
        );
        for m in &o.missed {
            eprintln!("  MISSED {m}");
        }
        assert!(
            o.missed.is_empty(),
            "{name}: {} deleted rows whose bytes survive were not recovered",
            o.missed.len()
        );
    }
}

/// secure_delete=ON zeroes deleted content. The same history as rollback.db
/// must yield nothing: no deleted row survives, and none may be invented.
#[test]
fn a_secure_delete_database_yields_nothing() {
    let dir = build();
    let truth: Json =
        serde_json::from_str(&std::fs::read_to_string(dir.join("truth.json")).unwrap()).unwrap();
    let o = check(&dir, &truth, "secure");
    eprintln!(
        "secure: {} deleted, {} with their body still in the files, {} recovered",
        o.deleted, o.surviving, o.recovered
    );
    assert!(o.deleted > 0);
    assert_eq!((o.surviving, o.recovered), (0, 0));
}

/// `read_table` returns exactly the live rows SQLite itself holds, rowid
/// included, for both journal modes.
#[test]
fn read_table_returns_the_live_rows() {
    let dir = build();
    let truth: Json =
        serde_json::from_str(&std::fs::read_to_string(dir.join("truth.json")).unwrap()).unwrap();
    for name in ["rollback", "wal"] {
        let db = &truth["databases"][name];
        let path = dir.join(db["file"].as_str().unwrap());
        let bytes = std::fs::read(&path).unwrap();
        let wal = std::fs::read(format!("{}-wal", path.display())).ok();
        let (cols, rows) = rc_sqlite_carve::read_table(&bytes, wal.as_deref(), "sms").unwrap();
        assert_eq!(cols.len(), 19);
        let got: BTreeSet<String> = rows
            .iter()
            .map(|r| serde_json::to_string(&as_json(r)).unwrap())
            .collect();
        let want: BTreeSet<String> = db["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["state"] == "live")
            .map(|r| serde_json::to_string(&r["values"]).unwrap())
            .collect();
        assert_eq!(got.len(), rows.len(), "{name}: duplicate rows");
        assert_eq!(got, want, "{name}");
    }
}
