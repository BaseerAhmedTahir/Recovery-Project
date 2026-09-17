//! `rc list-deleted` - recover deleted filenames, sizes, timestamps and the
//! original folder tree (SPEC.md section 8, Milestone 2).

use crate::output::{human_bytes, print_json, Table};
use clap::Args as ClapArgs;
use rc_fs::{DataLocation, EntryKind, EntryState};
use rc_partition::DiscoverOptions;
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct Args {
    /// Device or image file to scan (opened read-only).
    source: PathBuf,

    /// Scan the volume at this byte offset instead of discovering partitions.
    #[arg(long)]
    offset: Option<u64>,

    /// Scan this partition number from the discovered table.
    #[arg(long)]
    partition: Option<usize>,

    /// Also list files that are still present. Hidden by default because the
    /// point of the command is what was deleted.
    #[arg(long)]
    include_allocated: bool,

    /// Only show entries whose path or name contains this substring.
    #[arg(long)]
    filter: Option<String>,

    /// Show directories as well as files.
    #[arg(long)]
    include_dirs: bool,

    /// Maximum rows to print. 0 means no limit.
    #[arg(long, default_value_t = 200)]
    limit: usize,

    /// Override the source sector size.
    #[arg(long)]
    sector_size: Option<u32>,
}

#[derive(serde::Serialize)]
struct ListReport {
    source: String,
    volumes: Vec<VolumeReport>,
}

#[derive(serde::Serialize)]
struct VolumeReport {
    offset_bytes: u64,
    partition_origin: String,
    filesystem: String,
    deleted_count: usize,
    allocated_count: usize,
    damaged_count: usize,
    entries: Vec<EntryJson>,
    notes: Vec<String>,
    damaged: Vec<String>,
}

#[derive(serde::Serialize)]
struct EntryJson {
    name: String,
    path: Option<String>,
    path_confidence: String,
    kind: String,
    state: String,
    size: u64,
    created: Option<i64>,
    modified: Option<i64>,
    accessed: Option<i64>,
    /// False for FAT, whose timestamps carry no timezone at all.
    timestamps_utc_known: bool,
    /// How the file's bytes are located, and whether that is a fact or a guess.
    layout: String,
    layout_is_exact: bool,
    extents: usize,
    notes: Vec<String>,
}

pub fn run(args: Args, json: bool) -> anyhow::Result<()> {
    let sector_size = crate::parse_sector_size(args.sector_size)?;
    let device = rc_device::open(&args.source, sector_size)?;

    // Work out which volumes to scan.
    let offsets: Vec<(u64, String)> = if let Some(off) = args.offset {
        vec![(off, "explicit --offset".to_string())]
    } else {
        let mut table = rc_partition::discover(device.as_ref(), &DiscoverOptions::default())?;
        rc_partition::refine_filesystems(device.as_ref(), &mut table);

        if !json {
            print_partitions(&table);
        }

        let ss = device.sector_size();
        let chosen: Vec<_> = table
            .partitions
            .iter()
            .filter(|p| args.partition.map_or(true, |n| p.index == n))
            .filter(|p| p.fs.is_supported() || args.partition.is_some())
            .map(|p| {
                (
                    p.byte_offset(ss),
                    format!("partition {} ({}, {})", p.index, p.fs, p.origin),
                )
            })
            .collect();

        if chosen.is_empty() {
            // Say what was found, so an APFS or HFS+ volume gets its own
            // explanation instead of looking like an empty disk.
            for p in &table.partitions {
                if let Err(e @ rc_fs::FsError::NotImplemented { .. }) =
                    rc_fs::scan_volume(device.as_ref(), p.byte_offset(ss))
                {
                    anyhow::bail!("partition {}: {e}", p.index);
                }
            }
            anyhow::bail!(
                "no partition with a supported filesystem was found. \
                 Supported: NTFS, FAT12/16/32, exFAT, ext2/3/4. Use --offset to scan a \
                 specific byte offset anyway."
            );
        }
        chosen
    };

    let mut report = ListReport {
        source: args.source.to_string_lossy().to_string(),
        volumes: Vec::new(),
    };

    for (offset, origin) in offsets {
        match rc_fs::scan_volume(device.as_ref(), offset) {
            Ok((fs, result)) => {
                let deleted_count = result.deleted().count();
                let allocated_count = result.allocated().count();

                let mut entries: Vec<EntryJson> = Vec::new();
                for e in &result.entries {
                    if e.state == EntryState::Allocated && !args.include_allocated {
                        continue;
                    }
                    if e.kind == EntryKind::Directory && !args.include_dirs {
                        continue;
                    }
                    if let Some(f) = &args.filter {
                        let hay = e.path.clone().unwrap_or_else(|| e.name.clone());
                        if !hay.to_lowercase().contains(&f.to_lowercase()) {
                            continue;
                        }
                    }
                    entries.push(to_json(e));
                }
                entries.sort_by(|a, b| {
                    a.path
                        .as_deref()
                        .unwrap_or(&a.name)
                        .cmp(b.path.as_deref().unwrap_or(&b.name))
                });

                report.volumes.push(VolumeReport {
                    offset_bytes: offset,
                    partition_origin: origin,
                    filesystem: fs.to_string(),
                    deleted_count,
                    allocated_count,
                    damaged_count: result.damaged.len(),
                    entries,
                    notes: result.notes.clone(),
                    damaged: result.damaged.iter().take(20).cloned().collect(),
                });
            }
            Err(e) => {
                if json {
                    report.volumes.push(VolumeReport {
                        offset_bytes: offset,
                        partition_origin: origin,
                        filesystem: "unreadable".into(),
                        deleted_count: 0,
                        allocated_count: 0,
                        damaged_count: 0,
                        entries: Vec::new(),
                        notes: vec![e.to_string()],
                        damaged: Vec::new(),
                    });
                } else {
                    println!("\nvolume at byte {offset} ({origin}): {e}");
                }
            }
        }
    }

    if json {
        return print_json(&report);
    }

    for v in &report.volumes {
        print_volume(v, args.limit);
    }
    Ok(())
}

fn to_json(e: &rc_fs::Entry) -> EntryJson {
    let (layout, exact, extents) = describe_layout(&e.location);
    EntryJson {
        name: e.name.clone(),
        path: e.path.clone(),
        path_confidence: e.path_confidence.to_string(),
        kind: match e.kind {
            EntryKind::File => "file".into(),
            EntryKind::Directory => "dir".into(),
        },
        state: match e.state {
            EntryState::Deleted => "deleted".into(),
            EntryState::Allocated => "allocated".into(),
        },
        size: e.size,
        created: e.timestamps.created,
        modified: e.timestamps.modified,
        accessed: e.timestamps.accessed,
        timestamps_utc_known: e.timestamps.utc_known,
        layout,
        layout_is_exact: exact,
        extents,
        notes: e.notes.clone(),
    }
}

/// Describe where a file's bytes are, and be explicit about whether that is
/// known or assumed. This distinction is the whole point: a FAT deleted file's
/// "location" is a starting cluster and a hope.
fn describe_layout(loc: &DataLocation) -> (String, bool, usize) {
    match loc {
        DataLocation::Resident(d) => (format!("resident ({} bytes)", d.len()), true, 0),
        DataLocation::Runs(r) => (
            format!("{} extent{}", r.len(), if r.len() == 1 { "" } else { "s" }),
            true,
            r.len(),
        ),
        DataLocation::FirstClusterOnly(c) => (
            format!("first cluster {c} only (chain lost on delete)"),
            false,
            0,
        ),
        DataLocation::Unknown => ("unknown".into(), false, 0),
    }
}

fn print_partitions(table: &rc_partition::PartitionTable) {
    if table.partitions.is_empty() {
        println!("No partitions found.");
        return;
    }
    let mut t = Table::new(&["#", "start LBA", "sectors", "fs", "origin", "conf", "label"]);
    for p in &table.partitions {
        t.row(vec![
            p.index.to_string(),
            p.start.to_string(),
            p.sectors.to_string(),
            p.fs.to_string(),
            p.origin.to_string(),
            format!("{}%", p.confidence),
            p.label.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    t.print();

    for n in &table.notes {
        println!("note: {n}");
    }
    // An inferred partition is a hypothesis, and the operator should be told.
    for p in table.partitions.iter().filter(|p| p.origin.is_inferred()) {
        println!(
            "note: partition {} was inferred, not read from a partition table:",
            p.index
        );
        for e in &p.evidence {
            println!("        {e}");
        }
    }
    println!();
}

fn print_volume(v: &VolumeReport, limit: usize) {
    println!(
        "filesystem: {}  ({} deleted, {} allocated, {} damaged records)",
        v.filesystem, v.deleted_count, v.allocated_count, v.damaged_count
    );
    println!();

    if v.entries.is_empty() {
        println!("No matching entries.");
    } else {
        let mut t = Table::new(&["state", "size", "modified", "layout", "path"]);
        for e in v
            .entries
            .iter()
            .take(if limit == 0 { usize::MAX } else { limit })
        {
            let path = match (&e.path, e.path_confidence.as_str()) {
                (Some(p), "exact") | (Some(p), "traversed") => p.clone(),
                // Never present a guessed path as if it were known.
                (_, conf) => format!("?/{}  [{}]", e.name, conf),
            };
            t.row(vec![
                e.state.clone(),
                human_bytes(e.size),
                format_time(e.modified, e.timestamps_utc_known),
                e.layout.clone(),
                path,
            ]);
        }
        t.print();

        if limit != 0 && v.entries.len() > limit {
            println!(
                "\n({} more entries; pass --limit 0 to show all)",
                v.entries.len() - limit
            );
        }
    }

    println!();
    for n in &v.notes {
        println!("note: {n}");
    }
    for d in &v.damaged {
        println!("damaged: {d}");
    }
}

/// Format a Unix-nanosecond timestamp as UTC.
///
/// A trailing `?` marks a FAT timestamp, which has no timezone recorded, so
/// the instant is only as good as the writing machine's clock setting.
fn format_time(ns: Option<i64>, utc_known: bool) -> String {
    let Some(ns) = ns else {
        return "-".to_string();
    };
    let secs = ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}{}",
        tod / 3600,
        (tod % 3600) / 60,
        if utc_known { "" } else { "?" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_fs::Extent;

    #[test]
    fn layout_description_separates_fact_from_guess() {
        let (s, exact, n) = describe_layout(&DataLocation::Runs(vec![
            Extent::new(10, 4),
            Extent::new(20, 4),
        ]));
        assert_eq!(s, "2 extents");
        assert!(exact);
        assert_eq!(n, 2);

        let (s, exact, _) = describe_layout(&DataLocation::FirstClusterOnly(42));
        assert!(s.contains("chain lost"));
        assert!(
            !exact,
            "a FAT deleted file's layout is a guess and must not read as exact"
        );
    }

    #[test]
    fn timestamps_render_and_flag_missing_timezones() {
        // 2024-01-01T00:00:00Z
        let ns = 1_704_067_200i64 * 1_000_000_000;
        assert_eq!(format_time(Some(ns), true), "2024-01-01 00:00");
        assert_eq!(
            format_time(Some(ns), false),
            "2024-01-01 00:00?",
            "FAT timestamps must be visibly marked as timezone-less"
        );
        assert_eq!(format_time(None, true), "-");
    }
}
