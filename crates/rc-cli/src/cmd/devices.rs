//! `rc devices` - enumerate block devices.
//!
//! Enumeration is metadata-only and never reads sector data, so it works
//! without elevation on all three platforms. Devices that cannot be *read*
//! without elevation are still listed, with the reason in the `access` column,
//! because SPEC.md section 1 requires the UI to say what is actually wrong
//! rather than quietly omitting things.

use crate::output::{human_bytes, print_json, Table};
use clap::Args as ClapArgs;
use rc_device::{DeviceInfo, TrimSupport};

#[derive(ClapArgs)]
pub struct Args {
    /// Also show devices that cannot currently be read.
    #[arg(long, default_value_t = true)]
    all: bool,
}

#[derive(serde::Serialize)]
struct DevicesReport {
    elevated: bool,
    platform: &'static str,
    devices: Vec<DeviceJson>,
    notes: Vec<String>,
}

#[derive(serde::Serialize)]
struct DeviceJson {
    path: String,
    kind: String,
    model: Option<String>,
    serial: Option<String>,
    total_bytes: u64,
    sector_size: u32,
    total_sectors: u64,
    rotational: Option<bool>,
    trim: String,
    removable: Option<bool>,
    readable: bool,
    access_note: Option<String>,
    /// Plain-language expectation for recovery on this medium, from the table
    /// in SPEC.md section 1.1.
    recovery_outlook: String,
}

/// Honest expectation-setting, driven by what the device actually reports.
fn recovery_outlook(d: &DeviceInfo) -> String {
    match (d.trim, d.rotational, d.removable) {
        (TrimSupport::Yes, _, _) => {
            "poor - device reports TRIM; deleted data is usually zeroed by the controller"
                .to_string()
        }
        (_, Some(true), _) => {
            "good - mechanical drive; data persists until overwritten".to_string()
        }
        (_, Some(false), Some(true)) => {
            "good - removable flash usually has no TRIM; carving works well".to_string()
        }
        (_, Some(false), _) => {
            "uncertain - solid state; check whether TRIM is active on this volume".to_string()
        }
        _ => "unknown - could not determine media characteristics".to_string(),
    }
}

pub fn run(args: Args, json: bool) -> anyhow::Result<()> {
    let elevated = rc_device::is_elevated();
    let devices = rc_device::enumerate_devices()?;

    let mut notes = Vec::new();
    if !elevated && devices.iter().any(|d| !d.readable) {
        notes.push(
            if cfg!(windows) {
                "Some devices need an Administrator shell before their sectors can be read."
            } else {
                "Some devices need root before their sectors can be read."
            }
            .to_string(),
        );
    }
    if devices.is_empty() {
        notes.push(
            "No block devices were enumerated. Image files can still be scanned by path."
                .to_string(),
        );
    }

    if json {
        let report = DevicesReport {
            elevated,
            platform: std::env::consts::OS,
            devices: devices
                .iter()
                .map(|d| DeviceJson {
                    path: d.path.to_string_lossy().to_string(),
                    kind: d.kind.to_string(),
                    model: d.model.clone(),
                    serial: d.serial.clone(),
                    total_bytes: d.total_bytes(),
                    sector_size: d.sector_size.get(),
                    total_sectors: d.total_sectors,
                    rotational: d.rotational,
                    trim: d.trim.to_string(),
                    removable: d.removable,
                    readable: d.readable,
                    access_note: d.access_note.clone(),
                    recovery_outlook: recovery_outlook(d),
                })
                .collect(),
            notes,
        };
        return print_json(&report);
    }

    let mut table = Table::new(&[
        "path", "kind", "size", "sector", "model", "serial", "spin", "trim", "access",
    ]);
    for d in &devices {
        if !args.all && !d.readable {
            continue;
        }
        table.row(vec![
            d.path.to_string_lossy().to_string(),
            d.kind.to_string(),
            human_bytes(d.total_bytes()),
            d.sector_size.to_string(),
            d.model.clone().unwrap_or_else(|| "-".into()),
            d.serial.clone().unwrap_or_else(|| "-".into()),
            match d.rotational {
                Some(true) => "hdd".into(),
                Some(false) => "ssd".into(),
                None => "-".into(),
            },
            d.trim.to_string(),
            if d.readable {
                "ok".to_string()
            } else {
                d.access_note.clone().unwrap_or_else(|| "denied".into())
            },
        ]);
    }

    if table.is_empty() {
        println!("No devices found.");
    } else {
        table.print();
    }

    println!();
    println!(
        "elevated: {}   platform: {}",
        if elevated { "yes" } else { "no" },
        std::env::consts::OS
    );
    for n in &notes {
        println!("note: {n}");
    }

    // Only say something about recovery odds when we actually know something.
    for d in &devices {
        if d.trim == TrimSupport::Yes {
            println!(
                "note: {} reports TRIM. {}",
                d.path.display(),
                recovery_outlook(d)
            );
        }
    }

    Ok(())
}
