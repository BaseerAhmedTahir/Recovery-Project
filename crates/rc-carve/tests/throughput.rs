//! Milestone 3's last criterion: report MB/s, and have the number mean something.
//!
//! Every throughput figure this project produced before this file came from a
//! 512 MiB fixture that fits in RAM, so every one of them measured memory
//! bandwidth. That is not a benchmark, it is a number, and the difference is
//! what this file exists to fix.
//!
//! # Four tiers, because one figure hides which layer is the limit
//!
//! 0. **Raw read.** The same unbuffered 4 MiB reads the scanner's reader
//!    issues, with the bytes thrown away. What the disk hands the reader.
//! 1. **Engine.** The prefilter and header matching over bytes already in RAM,
//!    on one thread, with no I/O at all.
//! 2. **Page cache.** A full scan, buffered, of an image the OS has already
//!    cached. What every earlier figure actually was. Reported so it can be
//!    recognised, not so it can be quoted.
//! 3. **Device.** A full scan with the page cache bypassed, so every block
//!    comes off the disk. The number that describes a recovery.
//!
//! Comparing tier 3 against tier 0 is what says whether the reader or the
//! workers are the limit. Every tier is measured several times and reported as
//! a median with its range, because two consecutive single runs of this test
//! once disagreed by half.
//!
//! # What is and is not asserted
//!
//! Throughput is machine-dependent, so no speed is asserted - a threshold that
//! passes on this laptop fails on a slower CI runner and teaches everyone to
//! ignore it. What is asserted is correctness: the unbuffered scan must find
//! exactly what the buffered scan finds, which is the unbuffered read path
//! checked end to end through the whole scanner rather than one read at a time.
//!
//! **Release builds only.** A debug build optimises dependencies but not this
//! workspace, which is how the prefilter threshold was once set wrongly from a
//! measurement that compared an optimised `memchr` against an unoptimised loop.
//! Under a debug build this reports that the figures are not meaningful and
//! does not print them as if they were.

use rc_carve::scan::{scan, ScanOptions};
use rc_carve::{ScanIndex, SignatureDb};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

fn fixture() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("testdata")
        .join("fixtures")
        .join("quickformat.img");
    p.exists().then_some(p)
}

fn mib_per_sec(bytes: u64, secs: f64) -> f64 {
    if secs <= 0.0 {
        return 0.0;
    }
    bytes as f64 / (1024.0 * 1024.0) / secs
}

#[test]
fn throughput_in_three_tiers() {
    let Some(img) = fixture() else {
        eprintln!(
            "note: quickformat fixture not built; run testdata/build_fixtures.sh. \
             The benchmark needs a real image and cannot be substituted for."
        );
        return;
    };
    let lock_dir = img.parent().expect("fixture dir").to_path_buf();
    let _lock = rc_device::testutil::lock_fixtures(&lock_dir).ok();

    let db = SignatureDb::builtin().expect("builtin db");
    let bytes = std::fs::read(&img).expect("read fixture into RAM");
    let total = bytes.len() as u64;

    // --- tier 1: the engine, no I/O ----------------------------------------
    let index = ScanIndex::new(&db);
    let t = Instant::now();
    let mut matches = 0u64;
    index.prefilter.for_each_hit(&bytes, |at| {
        matches += index.candidates_at(&bytes, at).count() as u64;
    });
    let engine_secs = t.elapsed().as_secs_f64();
    let engine = mib_per_sec(total, engine_secs);
    std::hint::black_box(matches);

    // --- tiers 2 and 3: the full scan, cached and uncached ------------------
    //
    // Each measured several times and reported as a median. A single run is not
    // a benchmark: two consecutive runs of this test once reported the cached
    // scan at 509 and then 332 MiB/s, and a verdict computed from one of them
    // flipped between "mixed" and "CPU-bound" on noise alone.
    let opts = ScanOptions {
        validate: false,
        suppress_contained: false,
        ..Default::default()
    };
    let run = |unbuffered: bool| {
        let dev = if unbuffered {
            rc_device::open_unbuffered(&img, None).expect("open unbuffered")
        } else {
            rc_device::open(&img, None).expect("open buffered")
        };
        let end = dev.total_sectors() * dev.sector_size().get() as u64;
        let dev: Arc<dyn rc_device::ReadOnlyDevice> = Arc::from(dev);
        scan(dev, &db, 0, end, &opts).expect("scan")
    };
    const RUNS: usize = 5;
    let _warm = run(false);
    let mut cached_rates = Vec::new();
    let mut cached = None;
    for _ in 0..RUNS {
        let r = run(false);
        cached_rates.push(r.stats.mib_per_sec());
        cached = Some(r);
    }
    let cached = cached.expect("at least one cached run");
    let mut device_rates = Vec::new();
    let mut device = None;
    for _ in 0..RUNS {
        let r = run(true);
        device_rates.push(r.stats.mib_per_sec());
        device = Some(r);
    }
    let device = device.expect("at least one device run");

    // --- tier 0: what the disk hands the reader, with no scanning at all -----
    //
    // The same 4 MiB unbuffered sequential reads the scanner's reader issues,
    // doing nothing with the bytes. This is the ceiling the reader thread can
    // reach, and comparing the scan against it is what actually says whether
    // the reader or the workers are the limit - which is a question an earlier
    // version of this test answered with an unconditional guess.
    let raw_read = || {
        let dev = rc_device::open_unbuffered(&img, None).expect("open unbuffered");
        let ss = dev.sector_size().as_usize();
        let block = ScanOptions::default().block_bytes;
        let mut buf = rc_device::AlignedBuf::new(block, 4096);
        let total_sectors = dev.total_sectors();
        let per = (block / ss) as u64;
        let t = Instant::now();
        let mut lba = 0u64;
        let mut read = 0u64;
        while lba < total_sectors {
            let n = per.min(total_sectors - lba);
            let len = n as usize * ss;
            dev.read_exact_at(rc_device::Lba(lba), &mut buf[..len])
                .expect("raw read");
            read += len as u64;
            lba += n;
        }
        mib_per_sec(read, t.elapsed().as_secs_f64())
    };
    let mut raw_rates: Vec<f64> = (0..RUNS).map(|_| raw_read()).collect();

    let median = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 2]
    };
    let spread = |v: &[f64]| {
        let lo = v.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = v.iter().cloned().fold(0.0, f64::max);
        (lo, hi)
    };
    let (c_lo, c_hi) = spread(&cached_rates);
    let (d_lo, d_hi) = spread(&device_rates);
    let (r_lo, r_hi) = spread(&raw_rates);
    let cached_rate = median(&mut cached_rates);
    let device_rate = median(&mut device_rates);
    let raw_rate = median(&mut raw_rates);

    let debug = cfg!(debug_assertions);
    eprintln!("\n=== throughput, 512 MiB quick-formatted fixture, median of {RUNS} ===");
    if debug {
        eprintln!(
            "  DEBUG BUILD: the workspace is unoptimised and its dependencies are not, \
             so these figures are\n  not meaningful and are not reported. Run with \
             --release for a benchmark."
        );
    } else {
        eprintln!(
            "  0. raw read    {raw_rate:>7.0} MiB/s  [{r_lo:.0}-{r_hi:.0}]  unbuffered 4 MiB \
             reads, no scanning - the reader's ceiling"
        );
        eprintln!(
            "  1. engine      {engine:>7.0} MiB/s              prefilter + header match, \
             one thread, no I/O"
        );
        eprintln!(
            "  2. page cache  {cached_rate:>7.0} MiB/s  [{c_lo:.0}-{c_hi:.0}]  full scan, \
             buffered, warm - what every earlier figure really was"
        );
        eprintln!(
            "  3. device      {device_rate:>7.0} MiB/s  [{d_lo:.0}-{d_hi:.0}]  full scan, \
             unbuffered - the number that describes a recovery"
        );
        eprintln!(
            "     ({} threads, {} prefilter, block {} KiB)",
            device.stats.threads,
            device.stats.strategy,
            ScanOptions::default().block_bytes / 1024
        );

        // Which side is the limit, decided by measurement rather than asserted.
        // If the unbuffered scan runs near the raw read rate, the reader and
        // the disk are the limit. If it runs well below the raw read rate, the
        // disk has capacity to spare and the workers are what is holding it.
        let of_raw = device_rate / raw_rate.max(1e-9);
        let verdict = if of_raw > 0.85 {
            "I/O-bound: the scan runs at the rate the disk hands the reader"
        } else if of_raw < 0.5 {
            "CPU-bound: the disk delivers far more than the workers consume"
        } else {
            "mixed: the workers keep up with some but not all of what the disk delivers"
        };
        eprintln!(
            "  => {verdict}\n     (the unbuffered scan reaches {:.0}% of the raw read rate)",
            of_raw * 100.0
        );
    }

    // --- correctness: the only thing asserted -------------------------------
    let key = |r: &rc_carve::scan::ScanResult| {
        let mut v: Vec<(u64, String)> = r
            .candidates
            .iter()
            .map(|c| (c.offset, c.signature_id.clone()))
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        cached.stats.bytes_scanned, device.stats.bytes_scanned,
        "the unbuffered scan read a different number of bytes"
    );
    assert_eq!(
        cached.stats.header_matches, device.stats.header_matches,
        "the unbuffered scan found a different number of header matches"
    );
    assert!(
        key(&cached) == key(&device),
        "the unbuffered scan found different candidates from the buffered one"
    );
    assert_eq!(
        cached.stats.header_matches, matches,
        "the engine-only pass and the full scan disagree on how many headers there are"
    );
}
