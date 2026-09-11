//! `rc carve` - signature carving, with the numbers that make it judgeable.
//!
//! SPEC.md section 0 is explicit that the CLI is the source of truth, so
//! everything the scanner and the index can do is reachable here.
//!
//! The output is designed around one idea: **a carve is judged by its
//! denominator, not by its hit count.** Reporting "recovered 228 files" says
//! nothing without saying how many candidates had to be produced to get them.
//! So this prints candidates per GB scanned, how many survived validation, how
//! many were suppressed as contained inside a larger file, and the ratio of
//! candidates to files - and it prints them whether or not the carve went well.
//!
//! Throughput is reported with a warning attached when the scanned region is
//! small enough to have come from the page cache, because a figure measured
//! that way describes RAM rather than a device.

use crate::output::{human_bytes, print_json};
use clap::Args as ClapArgs;
use rc_carve::scan::{scan, ScanOptions, DEFAULT_BLOCK, DEFAULT_VALIDATION_WINDOW};
use rc_carve::SignatureDb;
use rc_index::CandidateIndex;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(ClapArgs)]
pub struct Args {
    /// Device or image file to carve. Opened read-only.
    source: PathBuf,

    /// Where to write the candidate index. Defaults to <source>.rcindex.
    #[arg(long)]
    index: Option<PathBuf>,

    /// Start carving at this byte offset.
    #[arg(long, default_value_t = 0)]
    start: u64,

    /// Stop at this byte offset. Defaults to the end of the device.
    #[arg(long)]
    end: Option<u64>,

    /// Only carve these formats, by signature id (repeatable). Narrowing the
    /// set usually narrows the prefilter too, which makes the scan much faster.
    #[arg(long = "only")]
    only: Vec<String>,

    /// Use a signature database from a file instead of the built-in one.
    #[arg(long)]
    signatures: Option<PathBuf>,

    /// Worker threads. The scan is I/O-bound on any real device, so more than a
    /// handful buys nothing.
    #[arg(long)]
    threads: Option<usize>,

    #[arg(long, default_value_t = DEFAULT_BLOCK)]
    block_bytes: usize,

    /// How much data a validator may see for one candidate. A file whose format
    /// permits more than this is validated on a truncated view.
    #[arg(long, default_value_t = DEFAULT_VALIDATION_WINDOW)]
    validation_window: usize,

    /// Emit raw header matches without validating them. Useful only to see what
    /// the validators are actually removing.
    #[arg(long)]
    no_validate: bool,

    /// Keep candidates that lie wholly inside a larger complete file, such as
    /// the EXIF thumbnail inside a photo.
    #[arg(long)]
    keep_contained: bool,

    /// Show the first N candidates.
    #[arg(long, default_value_t = 20)]
    show: usize,

    /// Read an image file with the operating system's page cache bypassed.
    ///
    /// Without this, a carve of an image that fits in RAM reports the speed of
    /// memory rather than of the disk, and a second run is faster than the
    /// first for no reason related to the engine. With it, the MiB/s figure
    /// describes the storage the image lives on. Raw devices are always read
    /// unbuffered, so this changes nothing for them.
    #[arg(long)]
    unbuffered: bool,
}

#[derive(serde::Serialize)]
struct CarveReport {
    source: String,
    index: String,
    bytes_scanned: u64,
    elapsed_secs: f64,
    mib_per_sec: f64,
    cache_warning: Option<String>,
    prefilter: &'static str,
    threads: usize,
    signatures: usize,
    prefilter_hits: u64,
    header_matches: u64,
    header_matches_per_gb: f64,
    validated: u64,
    rejected: u64,
    suppressed_contained: u64,
    window_capped: u64,
    window_artifact_lengths: u64,
    truncated: bool,
    candidates: u64,
    by_extension: BTreeMap<String, u64>,
    peak_rss_bytes: Option<u64>,
}

pub fn run(args: Args, json: bool) -> anyhow::Result<()> {
    let db = match &args.signatures {
        Some(p) => SignatureDb::from_file(p)?,
        None => SignatureDb::builtin()?,
    };

    let device = if args.unbuffered {
        rc_device::open_unbuffered(&args.source, None)?
    } else {
        rc_device::open(&args.source, None)?
    };
    let total = device.total_sectors() * device.sector_size().get() as u64;
    let end = args.end.unwrap_or(total).min(total);
    anyhow::ensure!(
        args.start < end,
        "nothing to scan: start {} is not before end {end}",
        args.start
    );

    let index_path = args.index.clone().unwrap_or_else(|| {
        let mut p = args.source.clone();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "carve".to_string());
        p.set_file_name(format!("{name}.rcindex"));
        p
    });

    // The index must not land on the device being carved: writing to a drive
    // you are recovering from overwrites the unallocated data you are trying to
    // recover. That check already exists, is already tested, and is the single
    // place SPEC.md section 4.1 wants the decision made - so call it rather
    // than writing a second one.
    //
    // My first attempt did write a second one, comparing what each path
    // resolves to, and it refused a perfectly safe carve on this machine: C:
    // and D: are the same physical disk, so an index on C: "resolved to the
    // same device" as an image file on D:. That is the wrong question. Writing
    // beside an image *file* is fine and ordinary - the filesystem will not
    // hand the index the image's blocks. Only a raw *device* source makes the
    // destination dangerous, and check_destination already knows the
    // difference.
    rc_image::check_destination(&index_path, rc_image::UnknownBackingPolicy::Warn)
        .map_err(|e| {
            anyhow::anyhow!(
                "{e}\n\nPass --index with a path on a different disk from the one \
                 being carved."
            )
        })?;

    let opts = ScanOptions {
        block_bytes: args.block_bytes,
        threads: args.threads,
        validate: !args.no_validate,
        validation_window: args.validation_window,
        only: if args.only.is_empty() {
            None
        } else {
            Some(args.only.clone())
        },
        suppress_contained: !args.keep_contained,
        ..Default::default()
    };

    if !json {
        println!(
            "carving {} from {} to {} ({})",
            args.source.display(),
            args.start,
            end,
            human_bytes(end - args.start)
        );
    }

    let device: Arc<dyn rc_device::ReadOnlyDevice> = Arc::from(device);
    let result = scan(Arc::clone(&device), &db, args.start, end, &opts)?;
    let s = &result.stats;

    let mut index = CandidateIndex::create(&index_path)?;
    index.extend(result.candidates.iter().cloned())?;
    index.finish()?;

    let mut by_extension: BTreeMap<String, u64> = BTreeMap::new();
    for c in &result.candidates {
        *by_extension.entry(c.ext.clone()).or_default() += 1;
    }

    if json {
        return print_json(&CarveReport {
            source: args.source.display().to_string(),
            index: index_path.display().to_string(),
            bytes_scanned: s.bytes_scanned,
            elapsed_secs: s.elapsed.as_secs_f64(),
            mib_per_sec: s.mib_per_sec(),
            cache_warning: if args.unbuffered {
                None
            } else {
                s.cache_warning().map(|w| w.to_string())
            },
            prefilter: s.strategy,
            threads: s.threads,
            signatures: s.signatures,
            prefilter_hits: s.prefilter_hits,
            header_matches: s.header_matches,
            header_matches_per_gb: s.header_matches_per_gb(),
            validated: s.validated,
            rejected: s.rejected,
            suppressed_contained: s.suppressed_contained,
            window_capped: s.window_capped,
            window_artifact_lengths: s.window_artifact_lengths,
            truncated: s.truncated,
            candidates: result.candidates.len() as u64,
            by_extension,
            peak_rss_bytes: rc_index::rss::peak_rss_bytes(),
        });
    }

    println!();
    println!(
        "scanned {} in {:.1}s = {:.0} MiB/s   ({} threads, {} prefilter, {} signatures)",
        human_bytes(s.bytes_scanned),
        s.elapsed.as_secs_f64(),
        s.mib_per_sec(),
        s.threads,
        s.strategy,
        s.signatures
    );
    if args.unbuffered {
        println!(
            "note: read unbuffered - this figure describes the storage device, not \
             the page cache"
        );
    } else if let Some(w) = s.cache_warning() {
        println!("note: {w} (pass --unbuffered to measure the disk instead)");
    }
    if cfg!(debug_assertions) {
        // The same trap that made the prefilter threshold wrong twice: a debug
        // build optimises dependencies and not this code, so a throughput
        // number from one is not comparable to anything.
        println!(
            "note: this is an unoptimised build, so the MiB/s figure is several times              lower than a release build would give. Use `cargo build --release`."
        );
    }

    println!();
    println!("what the scanner produced, before and after validation:");
    println!("  prefilter hits            {:>12}", s.prefilter_hits);
    println!(
        "  header matches            {:>12}   = {:.0} per GB scanned",
        s.header_matches,
        s.header_matches_per_gb()
    );
    println!(
        "  rejected by a validator   {:>12}   ({:.1}% of header matches survived)",
        s.rejected,
        s.validation_survival() * 100.0
    );
    println!(
        "  suppressed as contained   {:>12}   (thumbnails, images inside documents)",
        s.suppressed_contained
    );
    if s.window_capped > 0 {
        println!(
            "  validated on a part view  {:>12}   (format allows more than the {} window)",
            s.window_capped,
            human_bytes(args.validation_window as u64)
        );
    }
    if s.window_artifact_lengths > 0 {
        println!(
            "  length discarded          {:>12}   (was the window size, not a file length)",
            s.window_artifact_lengths
        );
    }
    println!("  candidates emitted        {:>12}", result.candidates.len());
    if s.truncated {
        println!("  WARNING: the candidate limit was reached; results are incomplete.");
    }

    println!();
    println!("by extension:");
    for (ext, n) in &by_extension {
        println!("  {ext:<10} {n:>10}");
    }

    if args.show > 0 && !result.candidates.is_empty() {
        println!();
        println!(
            "first {} candidates:",
            args.show.min(result.candidates.len())
        );
        println!(
            "  {:>14}  {:>12}  {:<8} {:<8} detail",
            "offset", "length", "ext", "status"
        );
        for c in result.candidates.iter().take(args.show) {
            let status = match c.status {
                rc_carve::Status::Valid => "valid",
                rc_carve::Status::Partial => "partial",
                rc_carve::Status::Rejected => "rejected",
            };
            let detail: String = c.detail.chars().take(60).collect();
            println!(
                "  {:>14}  {:>12}  {:<8} {:<8} {}",
                c.offset,
                if c.length > 0 {
                    c.length.to_string()
                } else {
                    "-".into()
                },
                c.ext,
                status,
                detail
            );
        }
    }

    println!();
    println!("index written to {}", index_path.display());
    if let Some(peak) = rc_index::rss::peak_rss_bytes() {
        println!("peak memory {}", rc_index::rss::format_bytes(peak));
    }
    println!(
        "\nNothing was written to {}. Carving is read-only.",
        args.source.display()
    );
    Ok(())
}
