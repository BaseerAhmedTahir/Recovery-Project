//! The safety test that gates the whole project (SPEC.md section 4.3).
//!
//! Hash a fixture image, drive the entire public surface of `rc-device`
//! against it, then hash it again. The two hashes must be identical.
//!
//! This runs in CI on every commit. It exists to catch the class of bug that
//! makes a recovery tool worse than useless: silently modifying the evidence
//! it was pointed at.
//!
//! The tests build their own scratch image when the generated fixtures are not
//! present, so CI is never silently skipped. When `testdata/fixtures/*.img`
//! do exist, they are covered too.

use rc_device::{open, DeviceError, Lba, ReadOnlyDevice, SectorSize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn sha256_file(path: &Path) -> String {
    use std::io::Read;
    let mut f = fs::File::open(path).expect("open image for hashing");
    let mut hasher = Sha256::new();
    // Explicit 4 MiB buffer rather than std::io::copy, which uses an 8 KiB
    // stack buffer. The fixtures total 4 GiB and on WSL they live on a drvfs
    // mount, where a million small reads takes tens of minutes instead of
    // seconds.
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    loop {
        let n = f.read(&mut buf).expect("read image for hashing");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hex::encode(hasher.finalize())
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/crates/rc-device
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn fixture_images() -> Vec<PathBuf> {
    let dir = workspace_root().join("testdata").join("fixtures");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("img"))
        .collect();
    out.sort();
    out
}

/// Build a self-contained scratch image with recognisable content.
///
/// Used so this test has something to run against even on a machine where the
/// generated fixtures have not been built.
fn scratch_image(name: &str, sectors: u64) -> PathBuf {
    let dir = std::env::temp_dir().join("rc-device-tests");
    fs::create_dir_all(&dir).expect("create scratch dir");
    let path = dir.join(name);

    let mut f = fs::File::create(&path).expect("create scratch image");
    for lba in 0..sectors {
        let mut sector = [0u8; 512];
        sector[..8].copy_from_slice(&lba.to_le_bytes());
        for (i, b) in sector.iter_mut().enumerate().skip(8) {
            *b = ((lba as usize).wrapping_mul(31).wrapping_add(i) % 251) as u8;
        }
        f.write_all(&sector).expect("write scratch sector");
    }
    f.sync_all().expect("sync scratch image");
    path
}

/// Exercise every public read path on a device.
///
/// The point is coverage of the API surface, not of any single read: if any
/// call were to open a write handle or fall back to a read-write mapping, the
/// hash comparison around this call would catch it.
fn exercise_full_api(dev: &dyn ReadOnlyDevice) {
    let ss = dev.sector_size();
    let total = dev.total_sectors();
    assert!(total > 0, "device reports zero sectors");

    // Metadata accessors.
    let _ = dev.info();
    let _ = dev.total_bytes();
    let _ = dev.serial();
    let _ = dev.is_rotational();
    let _ = dev.trim_supported();

    // Single sector at the start.
    let mut one = vec![0u8; ss.as_usize()];
    dev.read_at(Lba(0), &mut one).expect("read first sector");

    // Multi-sector read.
    let mut many = vec![0u8; ss.as_usize() * 8];
    dev.read_at(Lba(0), &mut many).expect("read 8 sectors");

    // Middle of the device.
    dev.read_at(Lba(total / 2), &mut one)
        .expect("read middle sector");

    // Final sector.
    dev.read_at(Lba(total - 1), &mut one)
        .expect("read last sector");

    // Reads that run off the end must clamp, not corrupt or panic.
    if total >= 4 {
        let mut tail = vec![0u8; ss.as_usize() * 4];
        let got = dev
            .read_at(Lba(total - 2), &mut tail)
            .expect("clamped tail read");
        assert_eq!(
            got,
            2 * ss.as_usize(),
            "tail read should clamp to the end of the device"
        );
    }

    // Unaligned byte-range reads through the bounce-buffer path.
    let mut bytes = vec![0u8; 100];
    dev.read_bytes_at(1, &mut bytes)
        .expect("unaligned read at 1");
    dev.read_bytes_at(ss.get() as u64 - 3, &mut bytes)
        .expect("read straddling a sector boundary");
    dev.read_bytes_at(0, &mut bytes).expect("aligned byte read");

    // Reading at/past the end returns 0 rather than erroring.
    let n = dev
        .read_bytes_at(dev.total_bytes(), &mut bytes)
        .expect("read at EOF");
    assert_eq!(n, 0, "reading at EOF should return zero bytes");

    // Range checks.
    assert!(dev.check_range(Lba(0), 1).is_ok());
    assert!(dev.check_range(Lba(total), 1).is_err());

    // Error paths must not touch the device either.
    let mut bad = vec![0u8; ss.as_usize() + 1];
    assert!(
        matches!(
            dev.read_at(Lba(0), &mut bad),
            Err(DeviceError::Misaligned { .. })
        ),
        "a non-sector-multiple buffer must be rejected"
    );
    let mut empty: Vec<u8> = Vec::new();
    assert!(dev.read_at(Lba(0), &mut empty).is_err());
    assert!(matches!(
        dev.read_at(Lba(total + 1000), &mut one),
        Err(DeviceError::OutOfRange { .. })
    ));
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// The core invariant, on a scratch image that always exists.
#[test]
fn scanning_never_modifies_the_source() {
    let img = scratch_image("immutability.img", 2048);
    let before = sha256_file(&img);

    {
        let dev = open(&img, None).expect("open scratch image");
        exercise_full_api(dev.as_ref());
    }

    let after = sha256_file(&img);
    assert_eq!(
        before, after,
        "image SHA-256 changed after a full read-only API pass"
    );
}

/// Every generated fixture, when they have been built.
#[test]
fn generated_fixtures_are_not_modified_by_scanning() {
    let images = fixture_images();
    if images.is_empty() {
        eprintln!(
            "note: no fixtures in testdata/fixtures; run testdata/build_fixtures.sh \
             for full coverage. The scratch-image test still ran."
        );
        return;
    }

    for img in images {
        let before = sha256_file(&img);
        {
            let dev = open(&img, None).unwrap_or_else(|e| panic!("open {}: {e}", img.display()));
            exercise_full_api(dev.as_ref());
        }
        let after = sha256_file(&img);
        assert_eq!(
            before,
            after,
            "fixture {} was modified by scanning",
            img.file_name().unwrap().to_string_lossy()
        );
    }
}

/// A full sequential sweep, which is what a real scan does.
#[test]
fn sequential_sweep_does_not_modify_the_source() {
    let img = scratch_image("sweep.img", 4096);
    let before = sha256_file(&img);

    {
        let dev = open(&img, None).expect("open scratch image");
        let ss = dev.sector_size().as_usize();
        let chunk = 64;
        let mut buf = vec![0u8; ss * chunk];
        let mut lba = 0u64;
        let total = dev.total_sectors();
        while lba < total {
            let n = (chunk as u64).min(total - lba) as usize;
            dev.read_at(Lba(lba), &mut buf[..n * ss])
                .expect("sequential read");
            lba += n as u64;
        }
    }

    assert_eq!(before, sha256_file(&img), "sweep modified the image");
}

/// Reads must return the bytes that are actually on disk, in the right place.
///
/// An immutability test alone would pass if reads returned nothing at all, so
/// this pins correctness of the read path against known sector content.
#[test]
fn reads_return_correct_content() {
    let img = scratch_image("content.img", 512);
    let dev = open(&img, None).expect("open scratch image");
    let ss = dev.sector_size().as_usize();

    for lba in [0u64, 1, 17, 255, 511] {
        let mut buf = vec![0u8; ss];
        dev.read_at(Lba(lba), &mut buf).expect("read sector");
        assert_eq!(
            u64::from_le_bytes(buf[..8].try_into().unwrap()),
            lba,
            "sector {lba} carries the wrong LBA marker"
        );
        assert_eq!(
            buf[100],
            ((lba as usize).wrapping_mul(31).wrapping_add(100) % 251) as u8,
            "sector {lba} body mismatch"
        );
    }

    // The unaligned path must agree with the aligned one.
    let mut whole = vec![0u8; ss * 2];
    dev.read_at(Lba(4), &mut whole).expect("read two sectors");
    let mut part = vec![0u8; 300];
    dev.read_bytes_at(4 * ss as u64 + 7, &mut part)
        .expect("unaligned read");
    assert_eq!(
        &part[..],
        &whole[7..307],
        "unaligned read disagrees with the aligned read of the same range"
    );
}

/// Opening a device registers it as a scan source, and closing unregisters it.
/// This is what `rc-image::OutputSink` relies on to refuse writing to a source.
#[test]
fn open_devices_register_as_scan_sources() {
    let img = scratch_image("registry.img", 128);
    let canonical = fs::canonicalize(&img).unwrap_or(img.clone());
    let id = rc_device::DeviceId::File(canonical.to_string_lossy().to_string());

    assert!(!rc_device::is_registered_source(&id));
    {
        let _dev = open(&img, None).expect("open scratch image");
        assert!(
            rc_device::is_registered_source(&id),
            "an open device must be registered as a scan source"
        );
        assert!(rc_device::registered_sources().contains(&id));
    }
    assert!(
        !rc_device::is_registered_source(&id),
        "closing a device must unregister it"
    );
}

/// A 4Kn image must be readable with a 4096-byte sector size.
#[test]
fn honours_explicit_sector_size() {
    let img = scratch_image("sectorsize.img", 512);
    let before = sha256_file(&img);

    let dev = open(&img, Some(SectorSize::S4096)).expect("open as 4Kn");
    assert_eq!(dev.sector_size(), SectorSize::S4096);
    assert_eq!(dev.total_sectors(), 512 * 512 / 4096);

    let mut buf = vec![0u8; 4096];
    dev.read_at(Lba(0), &mut buf).expect("read 4K sector");
    // The first 4096-byte sector spans the first eight 512-byte scratch sectors.
    assert_eq!(u64::from_le_bytes(buf[..8].try_into().unwrap()), 0);
    assert_eq!(u64::from_le_bytes(buf[512..520].try_into().unwrap()), 1);

    drop(dev);
    assert_eq!(before, sha256_file(&img));
}

/// The public API must expose no way to write. This is a compile-time property,
/// asserted here as documentation of intent: `rc-device` exports exactly one
/// device trait, and it has no write method.
#[test]
fn readonly_device_has_no_write_surface() {
    fn assert_read_only<T: ReadOnlyDevice + ?Sized>() {}
    assert_read_only::<dyn ReadOnlyDevice>();

    // If a write path is ever added to the trait, this test is the place the
    // reviewer should be forced to think about it.
    let img = scratch_image("nowrite.img", 64);
    let dev = open(&img, None).expect("open");
    let before = sha256_file(&img);
    let mut buf = vec![0u8; dev.sector_size().as_usize()];
    // Mutating our own buffer must never propagate to the device.
    dev.read_at(Lba(0), &mut buf).expect("read");
    buf.iter_mut().for_each(|b| *b = 0xFF);
    drop(dev);
    assert_eq!(
        before,
        sha256_file(&img),
        "mutating a read buffer must not reach the device"
    );
}
