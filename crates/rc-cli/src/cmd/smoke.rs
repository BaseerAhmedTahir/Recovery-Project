//! `rc smoke` - opt-in aligned-read check against real hardware.
//!
//! Everything else in Milestone 1 is verified against file-backed fixtures,
//! which never exercise `FILE_FLAG_NO_BUFFERING` / `O_DIRECT`. This subcommand
//! is the one place the unbuffered path touches a physical device, and it is
//! deliberately awkward to invoke:
//!
//! * Requires BOTH the `RC_SMOKE_ALLOW=1` environment variable AND an explicit
//!   `--device`. There is no auto-discovery and no default target, ever.
//! * Refuses to run against the system or boot drive.
//! * Reads a handful of sectors: LBA 0, plus a deliberately 4K-unaligned offset
//!   to prove the bounce-buffer alignment handling works.
//! * Hashes the exact byte range before and after and asserts it is unchanged,
//!   the same invariant the fixture immutability test enforces.
//! * Never runs in CI.
//!
//! Point it at a throwaway USB stick or SD card.

use crate::output::print_json;
use clap::Args as ClapArgs;
use rc_device::Lba;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const ALLOW_VAR: &str = "RC_SMOKE_ALLOW";

#[derive(ClapArgs)]
pub struct Args {
    /// Device to read, e.g. \\.\PhysicalDrive2 or /dev/sdb. Required; there is
    /// no default and no auto-discovery.
    #[arg(long)]
    device: String,

    /// Number of sectors to read at each probe point.
    #[arg(long, default_value_t = 8)]
    sectors: u64,

    /// Skip the system-drive guard. Refuses regardless on the actual boot disk;
    /// this only relaxes the heuristic checks.
    #[arg(long)]
    i_know_what_i_am_doing: bool,
}

#[derive(serde::Serialize)]
struct SmokeReport {
    device: String,
    sector_size: u32,
    total_sectors: u64,
    probes: Vec<Probe>,
    range_sha256_before: String,
    range_sha256_after: String,
    unchanged: bool,
    elevated: bool,
}

#[derive(serde::Serialize)]
struct Probe {
    label: String,
    offset_bytes: u64,
    requested: usize,
    got: usize,
    sha256: String,
}

pub fn run(args: Args, json: bool) -> anyhow::Result<()> {
    if std::env::var(ALLOW_VAR).ok().as_deref() != Some("1") {
        anyhow::bail!(
            "refusing to touch real hardware without explicit opt-in.\n\
             Set {ALLOW_VAR}=1 and pass --device <path>.\n\
             Point this at a throwaway USB stick or SD card, never your system drive."
        );
    }

    let path = PathBuf::from(&args.device);
    guard_system_drive(&path, args.i_know_what_i_am_doing)?;

    let device = rc_device::open(&path, None)?;
    let ss = device.sector_size();
    let total = device.total_sectors();
    anyhow::ensure!(total > 0, "device reports zero sectors");

    if !json {
        println!(
            "device: {} ({} sectors of {} bytes)",
            args.device, total, ss
        );
        println!("Reading a few sectors read-only. Nothing is written.\n");
    }

    // Probe points. The unaligned one is the whole reason this exists: it
    // exercises the bounce-buffer path that fixtures never reach.
    let n = args.sectors.min(total).max(1);
    let probes_spec: Vec<(String, u64, usize)> = vec![
        (
            "LBA 0 (aligned)".to_string(),
            0,
            (n * ss.get() as u64) as usize,
        ),
        (
            "4K-unaligned byte offset".to_string(),
            ss.get() as u64 + 3,
            1000,
        ),
        (
            "mid-device (aligned)".to_string(),
            (total / 2) * ss.get() as u64,
            ss.as_usize(),
        ),
    ];

    let mut probes = Vec::new();
    let mut before = Sha256::new();
    for (label, offset, len) in &probes_spec {
        let mut buf = vec![0u8; *len];
        let got = device.read_bytes_at(*offset, &mut buf)?;
        before.update(&buf[..got]);
        probes.push(Probe {
            label: label.clone(),
            offset_bytes: *offset,
            requested: *len,
            got,
            sha256: hex::encode(Sha256::digest(&buf[..got])),
        });
        if !json {
            println!(
                "  {:<26} offset {:>12}  read {:>5} of {:>5} bytes",
                label, offset, got, len
            );
        }
    }
    let range_before = hex::encode(before.finalize());

    // Read the same ranges again and confirm nothing changed underneath us.
    let mut after = Sha256::new();
    for (_, offset, len) in &probes_spec {
        let mut buf = vec![0u8; *len];
        let got = device.read_bytes_at(*offset, &mut buf)?;
        after.update(&buf[..got]);
    }
    let range_after = hex::encode(after.finalize());
    let unchanged = range_before == range_after;

    // A single-sector read must agree with the same bytes taken from a larger
    // aligned read: that is what proves the alignment handling is correct
    // rather than merely not crashing.
    let mut wide = vec![0u8; ss.as_usize() * 2];
    device.read_at(Lba(0), &mut wide)?;
    let mut narrow = vec![0u8; 100];
    device.read_bytes_at(ss.get() as u64 + 7, &mut narrow)?;
    let alignment_consistent = narrow[..] == wide[ss.as_usize() + 7..ss.as_usize() + 107];

    if json {
        print_json(&SmokeReport {
            device: args.device.clone(),
            sector_size: ss.get(),
            total_sectors: total,
            probes,
            range_sha256_before: range_before,
            range_sha256_after: range_after,
            unchanged,
            elevated: rc_device::is_elevated(),
        })?;
    } else {
        println!();
        println!("range sha256 (1st read): {range_before}");
        println!("range sha256 (2nd read): {range_after}");
        println!(
            "\nunchanged across reads:      {}",
            if unchanged { "yes" } else { "NO" }
        );
        println!(
            "unaligned matches aligned:   {}",
            if alignment_consistent { "yes" } else { "NO" }
        );
    }

    anyhow::ensure!(
        unchanged,
        "the same byte range hashed differently on a second read; \
         the read path is not stable on this device"
    );
    anyhow::ensure!(
        alignment_consistent,
        "an unaligned read disagreed with the aligned read of the same bytes; \
         the sector-alignment handling is wrong on this device"
    );

    if !json {
        println!("\nSMOKE TEST PASSED. The unbuffered read path works on this device.");
        println!("Record this in docs/LIMITATIONS.md.");
    }
    Ok(())
}

/// Refuse to touch the drive the OS is running from.
fn guard_system_drive(path: &std::path::Path, relaxed: bool) -> anyhow::Result<()> {
    let p = path.to_string_lossy().to_ascii_lowercase();

    // PhysicalDrive0 is not guaranteed to be the boot disk, but on a normal
    // machine it is, and getting this wrong is unrecoverable.
    if !relaxed && (p.ends_with("physicaldrive0") || p == "/dev/sda" || p == "/dev/disk0") {
        anyhow::bail!(
            "{} looks like the system drive. Refusing.\n\
             Point this at a removable device instead.",
            path.display()
        );
    }

    // Whatever backs the running executable is definitely in use.
    if let Ok(exe) = std::env::current_exe() {
        if let rc_image::Backing::Device { device, .. } = rc_image::resolve(&exe) {
            for d in device.split(',') {
                if d.trim().eq_ignore_ascii_case(path.to_string_lossy().trim()) {
                    anyhow::bail!(
                        "{} is the device this program is running from. Refusing.",
                        path.display()
                    );
                }
            }
        }
    }
    Ok(())
}
