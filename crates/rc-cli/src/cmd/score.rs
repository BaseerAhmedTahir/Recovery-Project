//! `rc score` - Green / Yellow / Red ratings for deleted files, with reasons.
//!
//! Every rating prints the rules that set it, and `--json` carries the full
//! input vector, so a rating can always be explained (SPEC.md 5.7).

use crate::output::{human_bytes, print_json};
use clap::Args as ClapArgs;
use rc_partition::DiscoverOptions;
use rc_score::{score_entry, Band, Context, Occupancy, Rules, Score};
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct Args {
    /// Device or image file (opened read-only).
    source: PathBuf,

    /// Score the volume at this byte offset instead of discovering partitions.
    #[arg(long)]
    offset: Option<u64>,

    /// Only show files in this band: green, yellow or red.
    #[arg(long)]
    band: Option<String>,

    /// Only show entries whose path contains this substring.
    #[arg(long)]
    filter: Option<String>,

    /// Use scoring rules from this file instead of the built-in ones.
    #[arg(long)]
    rules: Option<PathBuf>,

    /// Maximum rows to print. 0 means no limit.
    #[arg(long, default_value_t = 200)]
    limit: usize,
}

#[derive(serde::Serialize)]
struct Row {
    path: String,
    size: u64,
    score: Score,
}

pub fn run(args: Args, json: bool) -> anyhow::Result<()> {
    let device = rc_device::open(&args.source, None)?;
    let rules = match &args.rules {
        Some(p) => Rules::parse(&std::fs::read_to_string(p)?)?,
        None => Rules::builtin(),
    };
    let want_band = match args.band.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None => None,
        Some("green") => Some(Band::Green),
        Some("yellow") => Some(Band::Yellow),
        Some("red") => Some(Band::Red),
        Some(other) => anyhow::bail!("--band must be green, yellow or red, not {other}"),
    };

    let offsets: Vec<u64> = match args.offset {
        Some(o) => vec![o],
        None => {
            let mut table = rc_partition::discover(device.as_ref(), &DiscoverOptions::default())?;
            rc_partition::refine_filesystems(device.as_ref(), &mut table);
            let ss = device.sector_size();
            let found: Vec<u64> = table
                .partitions
                .iter()
                .filter(|p| p.fs.is_supported())
                .map(|p| p.byte_offset(ss))
                .collect();
            if found.is_empty() {
                vec![0]
            } else {
                found
            }
        }
    };

    let mut rows = Vec::new();
    let mut counts = [0usize; 3];
    for offset in offsets {
        let (fs, scan) = rc_fs::scan_volume(device.as_ref(), offset)?;
        let occupancy = Occupancy::from_scan(&scan);
        let ctx = Context::new(device.as_ref(), &scan, &occupancy, &rules, fs.to_string());
        for e in scan.deleted().filter(|e| e.kind == rc_fs::EntryKind::File) {
            let path = e.display_path();
            if let Some(f) = &args.filter {
                if !path.to_lowercase().contains(&f.to_lowercase()) {
                    continue;
                }
            }
            let score = score_entry(&ctx, e)?;
            counts[match score.band {
                Band::Green => 0,
                Band::Yellow => 1,
                Band::Red => 2,
            }] += 1;
            if want_band.is_some_and(|b| b != score.band) {
                continue;
            }
            rows.push(Row {
                path,
                size: e.size,
                score,
            });
        }
    }
    rows.sort_by(|a, b| a.score.value.cmp(&b.score.value).then(a.path.cmp(&b.path)));

    if json {
        return print_json(&rows);
    }
    println!(
        "{} GREEN, {} YELLOW, {} RED deleted files",
        counts[0], counts[1], counts[2]
    );
    let limit = if args.limit == 0 {
        usize::MAX
    } else {
        args.limit
    };
    for r in rows.iter().take(limit) {
        println!(
            "\n{:<6} {:>3}  {:>9}  {}",
            r.score.band,
            r.score.value,
            human_bytes(r.size),
            r.path
        );
        for reason in &r.score.reasons {
            println!("         {}: {}", reason.rule, reason.detail);
        }
    }
    Ok(())
}
