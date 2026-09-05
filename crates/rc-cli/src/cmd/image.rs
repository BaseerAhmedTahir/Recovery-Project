//! `rc image` - clone a device or image with ddrescue-style bad-sector handling.

use crate::output::{human_bytes, human_duration, print_json};
use clap::Args as ClapArgs;
use indicatif::{ProgressBar, ProgressStyle};
use rc_image::{clone_device, BlockStatus, CloneOptions, RescueOptions, SinkOptions};
use std::path::PathBuf;
use std::time::Duration;

#[derive(ClapArgs)]
pub struct Args {
    /// Device or image file to read (never opened for writing).
    source: PathBuf,

    /// Destination .raw file. Sidecar .map and .hash files sit beside it.
    output: PathBuf,

    /// Bulk read size for the first pass.
    #[arg(long, default_value = "1MiB", value_parser = parse_size)]
    block: u64,

    /// Extra passes over confirmed-bad sectors.
    #[arg(long, default_value_t = 0)]
    retries: u32,

    /// Byte used to fill unreadable regions.
    #[arg(long, default_value_t = 0)]
    fill: u8,

    /// Replace the output if it already exists.
    #[arg(long)]
    overwrite: bool,

    /// Continue a previous clone using its .map file.
    #[arg(long)]
    resume: bool,

    /// Write a dense image instead of a sparse one.
    #[arg(long)]
    no_sparse: bool,

    /// Refuse to start if the destination's physical device cannot be resolved.
    #[arg(long)]
    strict_destination: bool,

    /// Override the source sector size (for 4Kn images).
    #[arg(long)]
    sector_size: Option<u32>,

    /// Stop the first pass after this many failed blocks.
    #[arg(long)]
    max_error_blocks: Option<u64>,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let (num, mult) = if let Some(v) = t.strip_suffix("GiB") {
        (v, 1024 * 1024 * 1024)
    } else if let Some(v) = t.strip_suffix("MiB") {
        (v, 1024 * 1024)
    } else if let Some(v) = t.strip_suffix("KiB") {
        (v, 1024)
    } else {
        (t, 1)
    };
    num.trim()
        .parse::<u64>()
        .map(|n| n * mult)
        .map_err(|e| format!("invalid size {s:?}: {e}"))
}

#[derive(serde::Serialize)]
struct ImageReport {
    source: String,
    output: String,
    map: String,
    hash: String,
    total_bytes: u64,
    bytes_copied: u64,
    bad_bytes: u64,
    read_errors: u64,
    complete: bool,
    elapsed_secs: f64,
    mib_per_sec: f64,
    source_sha256: String,
    source_md5: String,
}

pub fn run(args: Args, json: bool) -> anyhow::Result<()> {
    let sector_size = crate::parse_sector_size(args.sector_size)?;
    let device = rc_device::open(&args.source, sector_size)?;
    let total = device.total_bytes();

    // Preflight the destination before doing any work, so the operator finds
    // out now rather than after a long copy.
    let policy = if args.strict_destination {
        rc_image::UnknownBackingPolicy::Refuse
    } else {
        rc_image::UnknownBackingPolicy::Warn
    };
    let backing = rc_image::check_destination(&args.output, policy)?;

    if !json {
        println!(
            "source:      {} ({}, {} sectors of {} bytes)",
            args.source.display(),
            human_bytes(total),
            device.total_sectors(),
            device.sector_size()
        );
        println!("destination: {}", args.output.display());
        match &backing {
            rc_image::Backing::Device { device: d, .. } => println!("             backed by {d}"),
            rc_image::Backing::Unknown { reason } => {
                println!("             backing device unknown ({reason})")
            }
        }
        println!();
    }

    let opts = CloneOptions {
        rescue: RescueOptions {
            block_bytes: args.block as usize,
            retries: args.retries,
            sparse: !args.no_sparse,
            fill_byte: args.fill,
            checkpoint_interval: Duration::from_secs(5),
            max_error_blocks: args.max_error_blocks,
        },
        sink: SinkOptions {
            allow_overwrite: args.overwrite,
            sparse: !args.no_sparse,
            unknown_backing: policy,
        },
        resume: args.resume,
    };

    let bar = if json {
        None
    } else {
        let b = ProgressBar::new(total);
        b.set_style(
            ProgressStyle::with_template("{bar:40} {bytes}/{total_bytes} ({bytes_per_sec}) {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        );
        Some(b)
    };

    let report = {
        let mut cb = |map: &rc_image::BlockMap, stats: &rc_image::RescueStats| {
            if let Some(b) = &bar {
                b.set_position(map.finished_bytes());
                let bad = map.bad_bytes();
                b.set_message(if bad > 0 {
                    format!(
                        "pass {} - {} unreadable",
                        map.current_pass,
                        human_bytes(bad)
                    )
                } else {
                    format!("pass {}", map.current_pass)
                });
            }
            let _ = stats;
            true // never cancels; Ctrl-C is handled by the process dying
        };
        clone_device(device.as_ref(), &args.output, &opts, Some(&mut cb))?
    };

    if let Some(b) = bar {
        b.finish_and_clear();
    }

    if json {
        return print_json(&ImageReport {
            source: args.source.to_string_lossy().to_string(),
            output: report.output.to_string_lossy().to_string(),
            map: report.map_path.to_string_lossy().to_string(),
            hash: report.hash_path.to_string_lossy().to_string(),
            total_bytes: total,
            bytes_copied: report.stats.bytes_copied,
            bad_bytes: report.bad_bytes,
            read_errors: report.stats.read_errors,
            complete: report.complete,
            elapsed_secs: report.stats.elapsed.as_secs_f64(),
            mib_per_sec: report.stats.throughput_mib_s(),
            source_sha256: report.source_digests.sha256.clone(),
            source_md5: report.source_digests.md5.clone(),
        });
    }

    println!("copied:   {}", human_bytes(report.stats.bytes_copied));
    println!(
        "elapsed:  {} ({:.1} MiB/s)",
        human_duration(report.stats.elapsed),
        report.stats.throughput_mib_s()
    );
    println!("sha256:   {}", report.source_digests.sha256);
    println!("md5:      {}", report.source_digests.md5);
    println!("map:      {}", report.map_path.display());
    println!("hash:     {}", report.hash_path.display());

    if report.complete {
        println!("\nClone is complete: every byte of the source was read.");
    } else {
        println!(
            "\nClone is INCOMPLETE: {} could not be read ({} read errors).",
            human_bytes(report.bad_bytes),
            report.stats.read_errors
        );
        println!(
            "Unreadable regions were filled with 0x{:02X}. The image hash therefore \
             does not match the physical device.",
            args.fill
        );
        println!(
            "Unreadable ranges are the '{}' entries in {}",
            BlockStatus::Bad.as_char(),
            report.map_path.display()
        );
    }

    Ok(())
}
