//! Partition discovery against the generated fixtures (SPEC.md section 5.3).
//!
//! The interesting case is `nopart`: a GPT-partitioned NTFS volume with both
//! copies of the partition table destroyed. Finding it requires scanning for
//! the filesystem's boot sector and rebuilding the table in memory.

use rc_partition::{DiscoverOptions, FsKind, Origin};
use std::path::PathBuf;

fn fixture(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("testdata")
        .join("fixtures")
        .join(format!("{name}.img"));
    p.exists().then_some(p)
}

/// Shared lock over the fixtures directory; see the note in rc-device's
/// immutability test. A concurrent `build_fixtures.sh` run would otherwise
/// make this report that discovery modified the source.
fn lock_fixtures() -> Option<rc_device::testutil::FixtureLock> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("testdata")
        .join("fixtures");
    rc_device::testutil::lock_fixtures(&dir).ok()
}

fn sha256(path: &std::path::Path) -> String {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap();
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    hex::encode(h.finalize())
}

/// The headline case: rebuild a destroyed partition table from scratch.
#[test]
fn rebuilds_a_wiped_partition_table_by_signature_scan() {
    let _lock = lock_fixtures();
    let Some(img) = fixture("nopart") else {
        eprintln!("note: nopart fixture not built");
        return;
    };
    let before = sha256(&img);

    let device = rc_device::open(&img, None).expect("open");
    let mut table =
        rc_partition::discover(device.as_ref(), &DiscoverOptions::default()).expect("discover");
    rc_partition::refine_filesystems(device.as_ref(), &mut table);

    drop(device);
    assert_eq!(before, sha256(&img), "discovery modified the image");

    eprintln!("\n=== nopart ===");
    for n in &table.notes {
        eprintln!("  note: {n}");
    }
    for p in &table.partitions {
        eprintln!(
            "  #{} start={} sectors={} fs={} origin={} confidence={}",
            p.index, p.start, p.sectors, p.fs, p.origin, p.confidence
        );
        for e in &p.evidence {
            eprintln!("        {e}");
        }
    }

    assert!(
        !table.primary_gpt_ok && !table.backup_gpt_ok,
        "both GPT copies were destroyed by the fixture generator"
    );

    let ntfs = table
        .partitions
        .iter()
        .find(|p| p.fs == FsKind::Ntfs)
        .expect("the NTFS volume should have been rediscovered");

    assert_eq!(
        ntfs.start.0, 2048,
        "the fixture puts the partition at the 1 MiB alignment boundary"
    );
    assert_eq!(
        ntfs.origin,
        Origin::Reconstructed,
        "with no table to read, this must be reported as inferred"
    );
    assert!(
        ntfs.origin.is_inferred(),
        "the CLI must be able to tell the operator this is a hypothesis"
    );
    assert!(!ntfs.evidence.is_empty(), "a guess must show its working");
}

/// A rediscovered partition must actually be usable.
#[test]
fn the_rebuilt_partition_can_be_parsed() {
    let Some(img) = fixture("nopart") else { return };
    let device = rc_device::open(&img, None).expect("open");
    let table =
        rc_partition::discover(device.as_ref(), &DiscoverOptions::default()).expect("discover");

    let ntfs = table
        .partitions
        .iter()
        .find(|p| p.fs == FsKind::Ntfs)
        .expect("found");
    let offset = ntfs.byte_offset(device.sector_size());

    // Reading the boot sector at the rebuilt offset must land on real NTFS.
    let mut sector = vec![0u8; 512];
    device.read_bytes_at(offset, &mut sector).expect("read");
    assert_eq!(
        &sector[3..11],
        b"NTFS    ",
        "the reconstructed offset does not point at a boot sector"
    );
}

/// A whole-device filesystem has no partition table at all, and saying
/// "reconstructed" there would be misleading.
#[test]
fn whole_device_filesystems_are_reported_as_such() {
    for (name, want) in [
        ("ntfs-basic", FsKind::Ntfs),
        ("fat32-basic", FsKind::Fat32),
        ("exfat-basic", FsKind::ExFat),
    ] {
        let Some(img) = fixture(name) else { continue };
        let device = rc_device::open(&img, None).expect("open");
        let mut table =
            rc_partition::discover(device.as_ref(), &DiscoverOptions::default()).expect("discover");
        rc_partition::refine_filesystems(device.as_ref(), &mut table);

        assert_eq!(table.partitions.len(), 1, "{name}: expected one volume");
        let p = &table.partitions[0];
        assert_eq!(p.origin, Origin::WholeDevice, "{name}");
        assert_eq!(
            p.start.0, 0,
            "{name}: a whole-device volume starts at LBA 0"
        );
        assert_eq!(
            p.fs, want,
            "{name}: filesystem identified from the boot sector"
        );
    }
}

/// Discovery must never modify the source.
#[test]
fn discovery_never_modifies_the_source() {
    let _lock = lock_fixtures();
    for name in [
        "ntfs-basic",
        "fat32-basic",
        "exfat-basic",
        "ext4-basic",
        "nopart",
    ] {
        let Some(img) = fixture(name) else { continue };
        let before = sha256(&img);
        {
            let device = rc_device::open(&img, None).expect("open");
            let opts = DiscoverOptions {
                always_scan: true,
                ..Default::default()
            };
            let mut t = rc_partition::discover(device.as_ref(), &opts).expect("discover");
            rc_partition::refine_filesystems(device.as_ref(), &mut t);
        }
        assert_eq!(before, sha256(&img), "{name} was modified by discovery");
    }
}

/// The scan must not invent partitions out of ordinary file data.
#[test]
fn a_full_scan_does_not_hallucinate_partitions() {
    let Some(img) = fixture("ntfs-basic") else {
        return;
    };
    let device = rc_device::open(&img, None).expect("open");
    let opts = DiscoverOptions {
        always_scan: true,
        ..Default::default()
    };
    let table = rc_partition::discover(device.as_ref(), &opts).expect("discover");

    // The fixture holds a corpus of JPEGs, PNGs and a 200 MiB filler region.
    // A signature scan that trusted magic bytes alone would report dozens of
    // spurious volumes.
    assert!(
        table.partitions.len() <= 3,
        "signature scan produced {} partitions on a single-volume image; \
         plausibility checks are too weak",
        table.partitions.len()
    );
}
