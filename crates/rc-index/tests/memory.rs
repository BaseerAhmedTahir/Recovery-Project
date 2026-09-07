//! The Milestone 3 memory criterion: peak RSS must not track candidate count.
//!
//! SPEC.md section 5.8 requires peak RSS under ~2 GB regardless of drive
//! size. That cannot be settled by reading the code - an index that batches
//! into a growing `Vec` before each commit is disk-backed in name and resident
//! in fact - so it is settled by the shape of a curve: grow the candidate count
//! tenfold and see whether memory follows.
//!
//! **Pass condition: ten times the candidates, no more than twice the peak.**
//! Sublinear with a named ratio rather than "flat", because SQLite's page
//! cache, the WAL and the connection itself all cost something, and an unnamed
//! bar is one that gets argued down the first time it fails.
//!
//! # The control
//!
//! A memory test that passes is worthless unless it could have failed. So this
//! runs the identical measurement against a deliberately resident index - a
//! plain `Vec<Candidate>` - and asserts that one *does* breach the ratio. If
//! the control stops failing, the measurement has lost its power and both
//! results are meaningless, whatever the real index reports.
//!
//! Peak RSS is a process-wide high-water mark and only ever rises, which is
//! exactly what makes this work: both measurements are taken as growth above
//! one baseline, so a disk-backed index leaves the second measurement equal to
//! the first while a resident one pushes it up.

use rc_carve::scan::Candidate;
use rc_carve::validate::Status;
use rc_carve::Category;
use rc_index::rss::{format_bytes, peak_rss_bytes};
use rc_index::CandidateIndex;

const SMALL: u64 = 50_000;
const LARGE: u64 = 500_000;
/// The ratio the criterion names.
const MAX_GROWTH_RATIO: f64 = 2.0;
/// SPEC.md section 5.8's absolute ceiling.
const CEILING: u64 = 2 << 30;

fn candidate(i: u64) -> Candidate {
    Candidate {
        offset: i * 4096,
        length: 8192,
        signature_id: "jpeg".into(),
        ext: "jpg".into(),
        category: Category::Image,
        status: Status::Valid,
        // Realistic payload: a real detail string and a real evidence vector,
        // because measuring with empty strings would understate the resident
        // case and flatter the disk-backed one.
        detail: "structurally complete; entropy scan reached EOI".into(),
        evidence: vec![
            ("width", "4032".to_string()),
            ("height", "3024".to_string()),
            ("restart_markers", "1512".to_string()),
        ],
        length_established: true,
    }
}

fn scratch() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("rc-index-mem-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

/// Peak growth, in bytes, caused by indexing `n` candidates to disk.
fn measure_disk_backed(dir: &std::path::Path, n: u64, baseline: u64) -> (u64, u64) {
    let path = dir.join(format!("index-{n}.db"));
    let mut ix = CandidateIndex::create(&path).expect("create index");
    for i in 0..n {
        ix.push(candidate(i)).expect("push");
    }
    ix.finish().expect("finish");
    let rows = ix.count().expect("count");
    assert_eq!(rows, n, "the index lost rows");
    drop(ix);

    let peak = peak_rss_bytes().expect("peak rss");
    let _ = std::fs::remove_file(&path);
    (peak.saturating_sub(baseline), peak)
}

/// The same measurement against something that is definitely resident.
fn measure_resident(n: u64, baseline: u64) -> u64 {
    let mut all: Vec<Candidate> = Vec::new();
    for i in 0..n {
        all.push(candidate(i));
    }
    // Touch it so nothing can be optimised away.
    let total: u64 = all.iter().map(|c| c.offset).sum();
    std::hint::black_box(total);
    let peak = peak_rss_bytes().expect("peak rss");
    drop(all);
    peak.saturating_sub(baseline)
}

#[test]
fn peak_memory_does_not_track_candidate_count() {
    let dir = scratch();

    // Warm the allocator and the SQLite library so the first real measurement
    // is not paying one-off costs that would flatter the ratio.
    {
        let path = dir.join("warmup.db");
        let mut ix = CandidateIndex::create(&path).expect("warmup");
        for i in 0..1000 {
            ix.push(candidate(i)).expect("push");
        }
        ix.finish().expect("finish");
        drop(ix);
        let _ = std::fs::remove_file(&path);
    }

    let baseline = peak_rss_bytes().expect("peak rss");

    let (small_growth, _) = measure_disk_backed(&dir, SMALL, baseline);
    let (large_growth, large_peak) = measure_disk_backed(&dir, LARGE, baseline);

    eprintln!("\n=== candidate index, peak RSS ===");
    eprintln!("  baseline           : {}", format_bytes(baseline));
    eprintln!(
        "  {SMALL} candidates  : +{} peak",
        format_bytes(small_growth)
    );
    eprintln!(
        "  {LARGE} candidates : +{} peak (absolute {})",
        format_bytes(large_growth),
        format_bytes(large_peak)
    );

    // A floor keeps the ratio meaningful when the disk-backed growth is so
    // small that measurement noise dominates it - which is the good outcome,
    // not a reason to divide by nearly zero.
    let floor = 8 << 20;
    let denom = small_growth.max(floor) as f64;
    let ratio = large_growth as f64 / denom;
    eprintln!(
        "  10x the candidates cost {ratio:.2}x the peak growth (bar: {MAX_GROWTH_RATIO:.1}x, \
         measured against a {} floor)",
        format_bytes(floor)
    );

    assert!(
        ratio <= MAX_GROWTH_RATIO,
        "10x the candidates grew peak RSS {ratio:.2}x, over the {MAX_GROWTH_RATIO:.1}x bar. \
         The index is holding rows in memory somewhere."
    );
    assert!(
        large_peak < CEILING,
        "peak RSS {} exceeds the {} ceiling from SPEC.md section 5.8",
        format_bytes(large_peak),
        format_bytes(CEILING)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Prove the measurement above can fail.
///
/// If a resident index does not breach the ratio, the test that passes is not
/// measuring anything, and its result says nothing about the real index.
#[test]
fn the_measurement_detects_an_index_that_is_actually_resident() {
    // Warm up, same as the real test.
    std::hint::black_box(measure_resident(1000, 0));
    let baseline = peak_rss_bytes().expect("peak rss");

    let small = measure_resident(SMALL, baseline);
    let large = measure_resident(LARGE, baseline);

    let floor = 8 << 20;
    let ratio = large as f64 / small.max(floor) as f64;
    eprintln!("\n=== control: a deliberately resident index ===");
    eprintln!("  {SMALL} candidates  : +{}", format_bytes(small));
    eprintln!("  {LARGE} candidates : +{}", format_bytes(large));
    eprintln!("  ratio {ratio:.2}x against the same {MAX_GROWTH_RATIO:.1}x bar");

    assert!(
        ratio > MAX_GROWTH_RATIO,
        "a Vec of {LARGE} candidates grew peak RSS only {ratio:.2}x over {SMALL}, which is \
         inside the bar the real test uses. The measurement has lost its power and \
         peak_memory_does_not_track_candidate_count is no longer evidence of anything."
    );
}
