//! The Android path against a SCRIPTED adb (`testdata/mobile/fake_adb.py`),
//! not a phone. It checks rc-mobile's side - the checklist, command parsing,
//! trashed-file discovery, pulling and the manifest - against output in the
//! formats adb and toybox print. Whether a real phone answers the same way is
//! not verified on this machine: no phone or emulator image is available
//! (LIMITATIONS).

use rc_mobile::adb::{checklist, Adb};
use rc_mobile::android::{pull_all, survey, trashed_mediastore_rows, Category};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

fn python() -> PathBuf {
    for py in ["python3", "python"] {
        if std::process::Command::new(py)
            .arg("--version")
            .output()
            .is_ok()
        {
            return PathBuf::from(py);
        }
    }
    panic!("Python is required for the scripted adb");
}

fn scenario(name: &str, devices: &str, dumpsys: &str) -> (PathBuf, Adb) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("adb-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("phone")).unwrap();
    let cfg = serde_json::json!({
        "devices": devices,
        "sdk": "34",
        "dumpsys_window": dumpsys,
        "content_query": "Row: 0 _id=41, _data=/storage/emulated/0/DCIM/Camera/.trashed-1726000000-IMG_20240901_101010.jpg, \
                          _display_name=.trashed-1726000000-IMG_20240901_101010.jpg, mime_type=image/jpeg, _size=5000, \
                          date_expires=1726000000, is_trashed=1\n",
    });
    std::fs::write(dir.join("scenario.json"), cfg.to_string()).unwrap();
    std::env::set_var("FAKE_ADB_SCENARIO", &dir);
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/mobile/fake_adb.py")
        .canonicalize()
        .unwrap();
    let adb = Adb {
        program: python(),
        prefix: vec![OsString::from(script)],
        serial: None,
    };
    (dir, adb)
}

fn put(root: &Path, remote: &str, bytes: &[u8]) {
    let p = root.join("phone").join(remote.trim_start_matches('/'));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

const ONE_DEVICE: &str = "List of devices attached\n\
    R58M123ABC             device usb:1-1 product:beyond1lteeea model:SM_G973F device:beyond1 transport_id:1\n\n";

/// The scenarios share the FAKE_ADB_SCENARIO variable, so they run in one test.
#[test]
fn checklist_survey_and_pull() {
    // Not authorized on the phone: nothing proceeds.
    let (_, adb) = scenario(
        "unauthorized",
        "List of devices attached\nR58M123ABC unauthorized usb:1-1 transport_id:1\n",
        "",
    );
    let c = checklist(Some(&adb), None);
    assert!(c.device_connected && !c.authorized && !c.ready(true));
    assert!(
        c.problems[0].contains("Allow USB debugging"),
        "{:?}",
        c.problems
    );

    // Lock screen showing: not ready, and a confirmation cannot override it.
    let (_, adb) = scenario("locked", ONE_DEVICE, "    mDreamingLockscreen=true\n");
    let c = checklist(Some(&adb), None);
    assert_eq!(c.unlocked, Some(false));
    assert!(!c.ready(true), "a locked phone must never be ready");

    // Lock state unknown: only with the holder's confirmation.
    let (_, adb) = scenario("unknown", ONE_DEVICE, "nothing about the keyguard\n");
    let c = checklist(Some(&adb), None);
    assert_eq!(c.unlocked, None);
    assert!(!c.ready(false) && c.ready(true));

    // Unlocked and authorized.
    let (dir, adb) = scenario(
        "ready",
        ONE_DEVICE,
        "KeyguardController:\n    mKeyguardShowing=false\n",
    );
    let c = checklist(Some(&adb), None);
    assert!(c.ready(false), "{:?}", c.problems);
    assert_eq!(c.api_level, Some(34));
    assert_eq!(c.model.as_deref(), Some("SM_G973F"));
    let adb = adb.with_serial("R58M123ABC");

    let trashed_photo: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
    put(
        &dir,
        "/storage/emulated/0/DCIM/Camera/.trashed-1726000000-IMG_20240901_101010.jpg",
        &trashed_photo,
    );
    put(
        &dir,
        "/storage/emulated/0/Movies/.trashed-1726500000-clip with space.mp4",
        b"movie bytes",
    );
    put(
        &dir,
        "/storage/emulated/0/DCIM/Camera/IMG_live.jpg",
        b"not trashed",
    );
    put(
        &dir,
        "/storage/emulated/0/DCIM/.thumbnails/1234.jpg",
        b"thumb",
    );
    put(
        &dir,
        "/storage/emulated/0/Android/media/com.whatsapp/WhatsApp/Media/WhatsApp Images/IMG-1.jpg",
        b"wa",
    );
    put(
        &dir,
        "/storage/emulated/0/Download/export/sms.db",
        b"SQLite format 3\0...",
    );

    let rows = trashed_mediastore_rows(&adb).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].size, Some(5000));

    let files = survey(&adb).unwrap();
    let by_cat = |c: Category| files.iter().filter(|f| f.category == c).count();
    assert_eq!(by_cat(Category::Trashed), 2, "{files:#?}");
    assert_eq!(by_cat(Category::Thumbnail), 1);
    assert_eq!(by_cat(Category::AppMedia), 1);
    assert_eq!(by_cat(Category::Database), 1);
    assert!(files.iter().all(|f| !f.path.ends_with("IMG_live.jpg")));
    let clip = files
        .iter()
        .find(|f| f.path.ends_with("clip with space.mp4"))
        .unwrap();
    assert_eq!(clip.original_name.as_deref(), Some("clip with space.mp4"));
    assert_eq!(clip.expires_unix, Some(1726500000));

    let out = dir.join("pulled");
    let pulled = pull_all(&adb, &files, &out).unwrap();
    assert_eq!(pulled.len(), files.len());
    for p in &pulled {
        assert!(p.size_matches, "{p:?}");
        let src = dir
            .join("phone")
            .join(p.remote.path.trim_start_matches('/'));
        assert_eq!(
            p.sha256,
            hex::encode(Sha256::digest(std::fs::read(src).unwrap()))
        );
        assert!(p.local.starts_with(&out));
    }
    assert!(out.join("manifest.json").is_file());
    // Never into a directory that already has something in it.
    assert!(pull_all(&adb, &files, &out).is_err());
}
