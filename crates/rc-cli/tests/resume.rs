//! Milestone 5 acceptance: "kill -9 mid-scan and resume to identical results".
//!
//! A real `rc carve` process on the 512 MiB quick-formatted fixture, slowed with
//! --max-rate-mib so it is still scanning when it is killed outright
//! (TerminateProcess on Windows, SIGKILL elsewhere - nothing gets to clean up),
//! then resumed from its checkpoint and compared, row by row and evidence by
//! evidence, with an uninterrupted carve of the same image.

use rc_index::CandidateIndex;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn fixture() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("testdata")
        .join("fixtures")
        .join("quickformat.img");
    assert!(p.exists(), "fixture quickformat is not built");
    p
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rc-resume-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rc() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rc"))
}

/// Every row of an index, without its row id, with its evidence.
fn rows(index: &Path) -> Vec<String> {
    let ix = CandidateIndex::open(index).expect("open index");
    ix.page(0, i64::MAX as usize)
        .expect("page")
        .into_iter()
        .map(|c| {
            format!(
                "{} {} {} {} {} {} {} {} {:?}",
                c.offset,
                c.length,
                c.signature_id,
                c.ext,
                c.category,
                c.status,
                c.length_established,
                c.detail,
                ix.evidence(c.id).unwrap()
            )
        })
        .collect()
}

fn next_offset(recstate: &Path) -> Option<(u64, bool)> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(recstate).ok()?).ok()?;
    Some((v["next_offset"].as_u64()?, v["complete"].as_bool()?))
}

#[test]
fn killed_mid_scan_and_resumed_gives_identical_results() {
    let img = fixture();
    let _lock = rc_device::testutil::lock_fixtures(img.parent().unwrap()).ok();
    let dir = scratch("kill");

    // The reference: one uninterrupted carve.
    let reference = dir.join("reference.rcindex");
    let out = rc()
        .args(["carve"])
        .arg(&img)
        .arg("--index")
        .arg(&reference)
        .arg("--show")
        .arg("0")
        .output()
        .expect("run rc");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let want = rows(&reference);
    assert!(want.len() > 200, "the reference carve found {}", want.len());

    // The victim: slowed down, small segments, killed partway.
    let victim = dir.join("victim.rcindex");
    let recstate = PathBuf::from(format!("{}.recstate", victim.display()));
    let mut child = rc()
        .args(["carve"])
        .arg(&img)
        .arg("--index")
        .arg(&victim)
        .args([
            "--segment-mib",
            "8",
            "--checkpoint-secs",
            "0.25",
            "--max-rate-mib",
            "48",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rc");
    let deadline = Instant::now() + Duration::from_secs(120);
    let killed_at = loop {
        if let Some((at, done)) = next_offset(&recstate) {
            assert!(!done, "the carve finished before it could be killed");
            if at >= 96 << 20 {
                break at;
            }
        }
        assert!(Instant::now() < deadline, "no checkpoint progress in time");
        std::thread::sleep(Duration::from_millis(50));
    };
    child.kill().expect("kill");
    let _ = child.wait();
    let (at_kill, done) = next_offset(&recstate).expect("checkpoint survives the kill");
    assert!(!done && at_kill >= killed_at && at_kill < 512 << 20);

    // Resume, and it must not start over.
    let out = rc()
        .args(["--json", "carve"])
        .arg(&img)
        .arg("--resume")
        .arg(&recstate)
        .output()
        .expect("resume");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json report");
    let from = report["resumed_from"].as_u64().expect("resumed_from");
    assert!(
        from >= at_kill,
        "resumed from {from}, but {at_kill} was committed"
    );
    assert_eq!(next_offset(&recstate), Some((512 << 20, true)));

    let got = rows(&victim);
    assert_eq!(got.len(), want.len(), "candidate count after resume");
    for (i, (a, b)) in got.iter().zip(&want).enumerate() {
        assert_eq!(a, b, "row {i} differs after kill and resume");
    }
    eprintln!(
        "killed at {} MiB, resumed from {} MiB, {} candidates identical",
        at_kill >> 20,
        from >> 20,
        got.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_checkpoint_is_refused_against_a_different_image() {
    let img = fixture();
    let other = img.with_file_name("ntfs-basic.img");
    assert!(other.exists(), "fixture ntfs-basic is not built");
    let _lock = rc_device::testutil::lock_fixtures(img.parent().unwrap()).ok();
    let dir = scratch("refuse");
    let index = dir.join("a.rcindex");
    let recstate = PathBuf::from(format!("{}.recstate", index.display()));

    let mut child = rc()
        .args(["carve"])
        .arg(&img)
        .arg("--index")
        .arg(&index)
        .args(["--segment-mib", "8", "--max-rate-mib", "32"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rc");
    let deadline = Instant::now() + Duration::from_secs(60);
    while next_offset(&recstate).map(|(a, _)| a).unwrap_or(0) == 0 {
        assert!(Instant::now() < deadline, "no checkpoint in time");
        std::thread::sleep(Duration::from_millis(50));
    }
    child.kill().unwrap();
    let _ = child.wait();

    let out = rc()
        .args(["carve"])
        .arg(&other)
        .arg("--resume")
        .arg(&recstate)
        .output()
        .expect("resume");
    assert!(
        !out.status.success(),
        "resuming against another image must fail"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("refusing to resume"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
