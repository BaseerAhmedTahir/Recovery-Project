//! The $MFT is read in large pieces, and the scan says where it has got to.
//!
//! Why this test exists: the first version read one MFT record per device
//! read. On a 512 MiB fixture that is invisible; on a real 728 GB volume it is
//! millions of 1 KiB unbuffered reads, and the person watching saw a progress
//! bar that never moved and a count that stayed at zero for ten minutes. Wall
//! clock on a fixture would not catch that coming back, but the number of
//! reads does.

use rc_device::testutil::FaultyDevice;
use rc_fs::{FsProgress, ScanCtx};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/fixtures/ntfs-basic.img")
}

#[test]
fn the_mft_is_read_in_chunks_and_reports_progress() {
    let image = fixture();
    assert!(image.exists(), "fixture ntfs-basic is not built");
    let _lock = rc_device::testutil::lock_fixtures(image.parent().unwrap()).ok();

    let inner = rc_device::open(&image, None).unwrap();
    let counting = FaultyDevice::new(inner, Vec::new());
    let volume = rc_fs::ntfs::NtfsVolume::open(&counting, 0).unwrap();
    let records = volume.record_count();
    assert!(records > 1000, "a fixture with only {records} MFT records");

    let mut seen: Vec<FsProgress> = Vec::new();
    let stop = AtomicBool::new(false);
    let mut cb = |p: FsProgress| seen.push(p);
    let scan = volume.scan_with(&mut ScanCtx::new(&stop, &mut cb)).unwrap();

    assert!(
        !scan.deleted().next().is_none(),
        "the fixture has deleted files"
    );

    // One read per record was the bug. A megabyte at a time is ~1024 records
    // per read; allow generous slack for runs, fix-ups and attribute lists,
    // and still fail loudly if per-record reading comes back.
    let reads = counting.read_count();
    eprintln!("{records} MFT records read with {reads} device reads");
    assert!(
        reads < records / 8,
        "{reads} device reads for {records} MFT records: the $MFT is being read a record at a \
         time again"
    );

    // Progress must move, name what it is doing, and end at the end.
    assert!(
        seen.len() >= 2,
        "progress was reported {} times",
        seen.len()
    );
    assert!(
        seen.iter()
            .any(|p| p.phase.contains("reading the list of files")),
        "{:?}",
        seen.first()
    );
    let last = seen.last().unwrap();
    assert_eq!(last.total, records.min(2_000_000));
    assert!(last.done > 0 && last.done <= last.total, "{last:?}");
}

#[test]
fn a_stopped_scan_keeps_what_it_found_and_says_so() {
    let image = fixture();
    let _lock = rc_device::testutil::lock_fixtures(image.parent().unwrap()).ok();
    let device = rc_device::open(&image, None).unwrap();
    let volume = rc_fs::ntfs::NtfsVolume::open(device.as_ref(), 0).unwrap();

    // Stop as soon as the first progress report arrives.
    let stop = AtomicBool::new(false);
    let mut first = true;
    let mut cb = |_: FsProgress| {
        if !first {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        first = false;
    };
    let scan = volume.scan_with(&mut ScanCtx::new(&stop, &mut cb)).unwrap();
    assert!(
        scan.notes.iter().any(|n| n.starts_with("stopped after")),
        "{:?}",
        scan.notes
    );
}
