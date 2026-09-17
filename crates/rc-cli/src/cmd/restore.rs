//! `rc restore` - write deleted files from a filesystem out to another disk.
//! `rc extract` - write carved candidates out, optionally reassembling the
//! fragmented ones.
//!
//! Both write only through `rc-restore`, which refuses any destination on the
//! device being read, and both record every file in `restore-manifest.json`
//! with its SHA-256, how its layout was known, and (for `restore`) its rating.

use crate::output::print_json;
use clap::Args as ClapArgs;
use rc_index::CandidateIndex;
use rc_partition::DiscoverOptions;
use rc_restore::{Layout, Restorer, Span};
use rc_score::{score_entry, Band, Context, Occupancy, Rules};
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct RestoreArgs {
    /// Device or image file (opened read-only).
    source: PathBuf,
    /// Output directory. Must not be on the device being read.
    #[arg(long)]
    out: PathBuf,
    /// Restore the volume at this byte offset instead of discovering partitions.
    #[arg(long)]
    offset: Option<u64>,
    /// Only files whose path contains this substring (case-insensitive).
    #[arg(long)]
    filter: Option<String>,
    /// Only files rated in these bands (repeatable): green, yellow, red.
    #[arg(long = "band")]
    bands: Vec<String>,
}

pub fn restore(args: RestoreArgs, json: bool) -> anyhow::Result<()> {
    let device = rc_device::open(&args.source, None)?;
    let mut restorer = Restorer::new(&args.out)?;
    let bands: Vec<Band> = args
        .bands
        .iter()
        .map(|b| match b.to_ascii_lowercase().as_str() {
            "green" => Ok(Band::Green),
            "yellow" => Ok(Band::Yellow),
            "red" => Ok(Band::Red),
            o => Err(anyhow::anyhow!(
                "--band must be green, yellow or red, not {o}"
            )),
        })
        .collect::<anyhow::Result<_>>()?;

    let offsets: Vec<u64> = match args.offset {
        Some(o) => vec![o],
        None => {
            let mut table = rc_partition::discover(device.as_ref(), &DiscoverOptions::default())?;
            rc_partition::refine_filesystems(device.as_ref(), &mut table);
            let ss = device.sector_size();
            let v: Vec<u64> = table
                .partitions
                .iter()
                .filter(|p| p.fs.is_supported())
                .map(|p| p.byte_offset(ss))
                .collect();
            if v.is_empty() {
                vec![0]
            } else {
                v
            }
        }
    };

    let rules = Rules::builtin();
    let mut skipped: Vec<(String, String)> = Vec::new();
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
            if !bands.is_empty() && !bands.contains(&score.band) {
                continue;
            }
            let mut notes = vec![format!("{} {}", score.band, score.value)];
            notes.extend(
                score
                    .reasons
                    .iter()
                    .map(|r| format!("{}: {}", r.rule, r.detail)),
            );
            if let Err(err) = restorer.entry(device.as_ref(), &scan.geometry, e, notes) {
                skipped.push((path, err.to_string()));
            }
        }
    }
    let written = restorer.finish()?;
    if json {
        return print_json(&serde_json::json!({"written": written, "skipped": skipped}));
    }
    println!(
        "wrote {} files into {} ({} skipped)",
        written.len(),
        args.out.display(),
        skipped.len()
    );
    for (p, why) in skipped.iter().take(20) {
        println!("  skipped {p}: {why}");
    }
    Ok(())
}

#[derive(ClapArgs)]
pub struct ExtractArgs {
    /// The device or image the index was carved from (opened read-only).
    source: PathBuf,
    /// The candidate index written by `rc carve`.
    #[arg(long)]
    index: PathBuf,
    /// Output directory. Must not be on the device being read.
    #[arg(long)]
    out: PathBuf,
    /// Only these extensions (repeatable).
    #[arg(long = "ext")]
    exts: Vec<String>,
    /// Also write candidates that did not validate completely, as carved.
    #[arg(long)]
    include_partial: bool,
    /// Try fragment reassembly for candidates that did not validate
    /// contiguously, and write the reassembled file when it validates.
    #[arg(long)]
    reassemble: bool,
}

pub fn extract(args: ExtractArgs, json: bool) -> anyhow::Result<()> {
    // Opening a missing index would create an empty one and report nothing.
    anyhow::ensure!(
        args.index.is_file(),
        "no candidate index at {}; run `rc carve` first",
        args.index.display()
    );
    let device = rc_device::open(&args.source, None)?;
    let index = CandidateIndex::open(&args.index)?;
    let sigs = rc_carve::SignatureDb::builtin()?;
    let mut restorer = Restorer::new(&args.out)?;
    let exts: Vec<String> = args.exts.iter().map(|e| e.to_ascii_lowercase()).collect();
    let (mut reassembled, mut failed) = (0usize, Vec::new());

    for c in index.page(0, usize::MAX)? {
        if !exts.is_empty() && !exts.contains(&c.ext.to_ascii_lowercase()) {
            continue;
        }
        let name = format!("{}/{:012x}.{}", c.ext, c.offset, c.ext);
        let source = format!("carved at byte {} ({})", c.offset, c.signature_id);
        if c.status == "valid" {
            restorer.spans(
                device.as_ref(),
                &name,
                &[Span {
                    offset: c.offset,
                    length: c.length,
                }],
                c.length,
                Layout::Carved,
                source,
                vec![format!("validated: {}", c.detail)],
            )?;
            continue;
        }
        let validator = sigs.get(&c.signature_id).and_then(|s| s.validator.clone());
        if args.reassemble {
            if let Some(v) = &validator {
                let r = rc_bifrag::reassemble(device.as_ref(), v, c.offset, &Default::default())?;
                if r.status == rc_carve::Status::Valid {
                    let spans: Vec<Span> = r
                        .pieces
                        .iter()
                        .map(|p| Span {
                            offset: p.offset,
                            length: p.length,
                        })
                        .collect();
                    restorer.spans(
                        device.as_ref(),
                        &name,
                        &spans,
                        r.length,
                        Layout::Reassembled,
                        source,
                        vec![format!(
                            "{} fragments; validated after reassembly",
                            r.fragments()
                        )],
                    )?;
                    reassembled += 1;
                    continue;
                }
                failed.push(format!("{}: {}", c.offset, r.detail));
            }
        }
        if args.include_partial {
            restorer.spans(
                device.as_ref(),
                &name,
                &[Span {
                    offset: c.offset,
                    length: c.length,
                }],
                c.length,
                Layout::Carved,
                source,
                vec![format!("NOT validated ({}): {}", c.status, c.detail)],
            )?;
        }
    }
    let written = restorer.finish()?;
    if json {
        return print_json(&serde_json::json!({
            "written": written,
            "reassembled": reassembled,
            "reassembly_failed": failed,
        }));
    }
    println!(
        "wrote {} files into {} ({} reassembled; {} reassembly attempts did not validate)",
        written.len(),
        args.out.display(),
        reassembled,
        failed.len()
    );
    Ok(())
}
