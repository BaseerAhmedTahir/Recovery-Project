//! Milestone 8 acceptance, backend half: "grid stays responsive at 10M
//! synthetic rows". The grid asks for one screen of rows at a time; this
//! measures exactly those requests at 10 million rows, at random positions,
//! unfiltered and after a filter and a sort, and how long building the
//! filtered view takes (which runs off the UI thread).
//!
//! The budget for a screen of rows is one frame, 16 ms. The assertion is on the
//! median, with a 100 ms ceiling on the 99th percentile, because a machine that
//! is compiling at the same time can push one query past 16 ms with nothing
//! wrong in the code. The p99 figures quoted in the docs were measured on an
//! otherwise idle machine and are printed by this test.
//!
//! The frontend half - that the page renders only visible rows - is measured in
//! the browser (docs/PROGRESS.md).

use rc_results::{Filter, Store};
use std::time::{Duration, Instant};

const ROWS: u64 = 10_000_000;
const SCREEN: u64 = 60;

fn p99(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[(v.len() * 99 / 100).min(v.len() - 1)]
}

fn p50(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// The 99th percentile and the median of one screen's fetch.
fn sample(store: &Store, n: u64) -> (Duration, Duration) {
    let len = store.view_len();
    let mut x: u64 = 12345;
    let mut times = Vec::new();
    for _ in 0..500 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let start = (x >> 11) % len.saturating_sub(n).max(1);
        let t = Instant::now();
        let rows = store.rows(start, n).unwrap();
        times.push(t.elapsed());
        assert_eq!(rows.len() as u64, n.min(len - start));
    }
    (p99(times.clone()), p50(times))
}

#[test]
fn a_screen_of_rows_at_ten_million_fits_in_a_frame() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ten-million");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut store = Store::open(&dir.join("results.sqlite")).unwrap();

    let t = Instant::now();
    store.add_synthetic(ROWS).unwrap();
    let insert = t.elapsed();
    assert_eq!(store.view_len(), ROWS);

    let (unfiltered, unfiltered_mid) = sample(&store, SCREEN);

    let t = Instant::now();
    let n = store
        .set_view(&Filter {
            bands: vec!["RED".into()],
            exts: vec!["jpg".into(), "png".into()],
            ..Default::default()
        })
        .unwrap();
    let filter_build = t.elapsed();
    assert!(n > 100_000, "the filter matched only {n}");
    let (filtered, filtered_mid) = sample(&store, SCREEN);

    let t = Instant::now();
    let m = store
        .set_view(&Filter {
            text: "dir_004".into(),
            sort: Some("size".into()),
            descending: true,
            ..Default::default()
        })
        .unwrap();
    let sort_build = t.elapsed();
    let sorted_rows = store.rows(0, 3).unwrap();
    assert!(sorted_rows[0].size >= sorted_rows[1].size);
    assert!(sorted_rows[1].size >= sorted_rows[2].size);
    let (sorted, sorted_mid) = sample(&store, SCREEN);

    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    eprintln!(
        "10M rows: insert {:.1}s; a screen of {SCREEN} rows, median/p99 in ms: \
         unfiltered {:.2}/{:.2}, filtered ({n} rows, view built in {:.1}s) {:.2}/{:.2}, \
         text+sort ({m} rows, view built in {:.1}s) {:.2}/{:.2}",
        insert.as_secs_f64(),
        ms(unfiltered_mid),
        ms(unfiltered),
        filter_build.as_secs_f64(),
        ms(filtered_mid),
        ms(filtered),
        sort_build.as_secs_f64(),
        ms(sorted_mid),
        ms(sorted),
    );

    // The median must fit in a frame. The 99th percentile gets a far looser
    // ceiling: this machine may be compiling something else at the time, and a
    // benchmark that fails for that reason is one people learn to ignore. A
    // p99 past 100 ms would be a regression in the query, not load.
    let frame = Duration::from_millis(16);
    let ceiling = Duration::from_millis(100);
    for (what, mid, p99) in [
        ("unfiltered", unfiltered_mid, unfiltered),
        ("filtered", filtered_mid, filtered),
        ("sorted", sorted_mid, sorted),
    ] {
        assert!(mid < frame, "{what}: median {mid:?} is over one frame");
        assert!(p99 < ceiling, "{what}: p99 {p99:?} is past the ceiling");
    }
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
