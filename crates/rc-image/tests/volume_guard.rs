//! Scanning a drive letter must keep recovered files off that volume.
//!
//! Opening `\\.\D:` for reading needs Administrator, which a test run does not
//! have, so this registers the scan source exactly as `WindowsDevice::open`
//! does for a volume - under the drive-letter path and under the volume's
//! `\\?\Volume{GUID}` name - and then asks the real destination check about
//! real folders on this machine.
//!
//! Before volumes were registered under their GUID, scanning `\\.\D:` and
//! writing to `D:\recovered` was allowed: the destination resolved to
//! `\\.\PhysicalDriveN`, which never equals `\\.\D:`.

#![cfg(windows)]

use rc_device::testutil::register_source;
use rc_device::volumes::volume_guid;
use rc_device::DeviceId;
use rc_image::sink::{check_destination, UnknownBackingPolicy};
use std::path::{Path, PathBuf};

fn letter_of(p: &Path) -> char {
    p.to_string_lossy()
        .chars()
        .next()
        .unwrap()
        .to_ascii_uppercase()
}

#[test]
fn a_scanned_volume_refuses_destinations_on_itself_only() {
    // The volume this test's target directory is on.
    let here = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let letter = letter_of(&here);
    let device = format!("\\\\.\\{letter}:");
    let guid = volume_guid(&device).expect("a volume GUID for the target directory's volume");

    // What `WindowsDevice::open` registers for a volume.
    let _src = register_source(DeviceId::Path(device.clone()), Path::new(&device));
    let _alias = register_source(DeviceId::Path(guid.clone()), Path::new(&guid));

    let dest = here.join("recovered-here");
    let err = check_destination(&dest, UnknownBackingPolicy::Refuse)
        .expect_err("writing onto the volume being scanned must be refused");
    assert!(
        err.to_string().contains("being scanned") || err.to_string().contains("source"),
        "{err}"
    );

    // A different lettered volume is a different set of sectors, even on the
    // same physical disk, and stays writable.
    let other = rc_device::enumerate_volumes()
        .into_iter()
        .find(|v| v.letter != letter && v.filesystem.is_some() && !v.removable);
    match other {
        Some(v) => {
            let dest = PathBuf::from(&v.mount).join("recovered-elsewhere");
            check_destination(&dest, UnknownBackingPolicy::Refuse)
                .unwrap_or_else(|e| panic!("{} should be allowed: {e}", dest.display()));
        }
        None => eprintln!(
            "only one fixed volume here; the 'other volume is allowed' half is not exercised"
        ),
    }
}
