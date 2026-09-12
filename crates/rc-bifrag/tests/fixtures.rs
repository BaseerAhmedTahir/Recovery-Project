//! Milestone 4: reassemble the fragmented fixtures, and say honestly how well.
//!
//! Two fixtures, and the difference between them is the whole point.
//!
//! **`fragmented-jpeg`** has constant filler in every gap, and every gap the
//! same size. Its gaps have zero entropy against file data at nearly eight
//! bits per byte, so skipping uniform clusters would reassemble everything
//! without understanding any format. It is a sanity check. A reassembler that
//! passes it has shown it does not break things, and nothing else.
//!
//! **`fragmented-hard`** fills every gap with a slice of other real media from
//! the same encoders with the same settings - the same camera, in effect - and
//! varies the gaps from one cluster to 256. A gap then looks like the file it
//! interrupts, which is what a real card looks like. This is the result that
//! means something.
//!
//! Every file in both comes from ImageMagick or ffmpeg, not from this
//! project's own encoders: reassembly asks a validator over and over whether
//! a splice still decodes, and grading that against files written by the same
//! reading of the spec would grade nothing.
//!
//! Each file is reassembled starting from its true header, so the measurement
//! is of reassembly and not of scanning. It is graded two ways: by hash - did
//! the whole file come back, byte for byte - and against the fixture's recorded
//! fragment layout, which says how close a miss was.
//!
//! The baseline Milestone 4 has to improve on is contiguous-only carving, which
//! recovers none of these files: every one is fragmented, and that is checked
//! here too rather than assumed.

use rc_bifrag::{assemble, reassemble, Method, Options};
use rc_carve::validate::Status;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::Instant;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("testdata")
        .join("fixtures")
}

fn validator_for(kind: &str) -> &'static str {
    match kind {
        "jpeg" => "jpeg",
        "mp4" => "mp4",
        "png" => "png",
        other => panic!("no validator for {other}"),
    }
}

struct Row {
    file: String,
    kind: String,
    true_fragments: usize,
    found_fragments: usize,
    pieces_correct: usize,
    recovered: bool,
    contiguous_recovers: bool,
    status: Status,
    method: Method,
    calls: u64,
    gib: f64,
    prefiltered: u64,
    backtracks: u64,
    window: u64,
    millis: u128,
    /// For a miss: where the found layout first departs from the true one, and
    /// what the reassembler said when it stopped.
    miss: Option<String>,
}

/// Describe where a reassembly went wrong, against the recorded layout.
fn divergence(found: &[(u64, u64)], truth: &[(u64, u64)], detail: &str) -> String {
    let k = found.iter().zip(truth).take_while(|(a, b)| a == b).count();
    let show = |p: Option<&(u64, u64)>| match p {
        Some((o, l)) => format!("{o}+{l}"),
        None => "(none)".to_string(),
    };
    format!(
        "piece {k}: found {} where the truth is {}; {detail}",
        show(found.get(k)),
        show(truth.get(k))
    )
}

fn run(name: &str) -> Vec<Row> {
    let img = fixture_dir().join(format!("{name}.img"));
    let json = fixture_dir().join(format!("{name}.expected.json"));
    // Fail, don't skip: a reassembly suite that quietly passes when its
    // fixtures are missing is worse than none.
    assert!(
        img.exists() && json.exists(),
        "fixture {name} is not built; run\n  ./testdata/build_fixtures.sh {name}"
    );
    let _lock = rc_device::testutil::lock_fixtures(&fixture_dir()).ok();
    let truth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).expect("read")).expect("parse");
    let layout = truth["layout"]["files"]
        .as_object()
        .unwrap_or_else(|| panic!("{name}: no recorded layout; rebuild the fixture"));
    let cluster = truth["layout"]["cluster_bytes"].as_u64().unwrap_or(4096);

    let dev = rc_device::open(&img, None).expect("open fixture");
    let opts = Options {
        cluster,
        ..Default::default()
    };

    let mut rows = Vec::new();
    for (path, meta) in truth["files"].as_object().expect("files") {
        let kind = meta["kind"].as_str().unwrap_or_default().to_string();
        let want_sha = meta["sha256"].as_str().unwrap_or_default().to_string();
        let size = meta["size"].as_u64().unwrap_or(0);
        let true_frags: Vec<(u64, u64)> = layout[path]["fragments"]
            .as_array()
            .expect("fragments")
            .iter()
            .map(|f| (f["offset"].as_u64().unwrap(), f["length"].as_u64().unwrap()))
            .collect();
        let header = true_frags[0].0;

        // The baseline: the bytes as they lie on the disk from the header.
        let mut flat = vec![0u8; size as usize];
        let n = dev.read_bytes_at(header, &mut flat).unwrap_or(0);
        let contiguous_recovers = hex::encode(Sha256::digest(&flat[..n])) == want_sha;

        let started = Instant::now();
        let r = reassemble(&*dev, validator_for(&kind), header, &opts).expect("reassemble");
        let millis = started.elapsed().as_millis();
        let bytes = assemble(&*dev, &r).expect("assemble");
        let recovered = hex::encode(Sha256::digest(&bytes)) == want_sha;

        let found: Vec<(u64, u64)> = r.pieces.iter().map(|p| (p.offset, p.length)).collect();
        let pieces_correct = found
            .iter()
            .zip(&true_frags)
            .take_while(|(a, b)| a == b)
            .count();

        rows.push(Row {
            file: path.rsplit('/').next().unwrap_or(path).to_string(),
            kind,
            true_fragments: true_frags.len(),
            found_fragments: found.len(),
            pieces_correct,
            recovered,
            contiguous_recovers,
            status: r.status,
            method: r.method,
            calls: r.validator_calls,
            gib: r.validated_bytes as f64 / (1u64 << 30) as f64,
            prefiltered: r.prefiltered,
            backtracks: r.backtracks,
            window: r.window,
            millis,
            miss: (!recovered).then(|| divergence(&found, &true_frags, &r.detail)),
        });
    }
    rows
}

fn report(name: &str, rows: &[Row]) {
    eprintln!("\n=== {name} ===");
    eprintln!(
        "  {:<12} {:<5} {:>5} {:>5} {:>5} {:>7} {:>6} {:>5} {:>5} {:>6} {:>7}  result",
        "file", "kind", "frags", "found", "right", "calls", "GiB", "skip", "back", "window", "ms"
    );
    for r in rows {
        let result = match (r.recovered, r.status) {
            (true, _) => "RECOVERED".to_string(),
            // A wrong file the validator calls complete is the worst outcome
            // there is, and it gets its own word.
            (false, Status::Valid) => "WRONG but Valid".to_string(),
            (false, s) => format!("no ({s:?})"),
        };
        eprintln!(
            "  {:<12} {:<5} {:>5} {:>5} {:>5} {:>7} {:>6.2} {:>5} {:>5} {:>6} {:>7}  {}",
            r.file,
            r.kind,
            r.true_fragments,
            r.found_fragments,
            r.pieces_correct,
            r.calls,
            r.gib,
            r.prefiltered,
            r.backtracks,
            r.window,
            r.millis,
            result
        );
    }
    for r in rows {
        if let Some(m) = &r.miss {
            eprintln!("  {}: {m}", r.file);
        }
        if r.recovered {
            if let Method::Sequential { fragments } = r.method {
                assert_eq!(
                    fragments, r.true_fragments,
                    "{}: recovered, so the pieces must be the true ones",
                    r.file
                );
            }
        }
    }
    let recovered = rows.iter().filter(|r| r.recovered).count();
    let baseline = rows.iter().filter(|r| r.contiguous_recovers).count();
    eprintln!(
        "  -> {recovered} of {} recovered byte-exactly; contiguous-only baseline {baseline}",
        rows.len()
    );
}

#[test]
fn reassembly_on_both_fragmented_fixtures() {
    let easy = run("fragmented-jpeg");
    let hard = run("fragmented-hard");
    report(
        "fragmented-jpeg (constant-fill gaps: a sanity check)",
        &easy,
    );
    report(
        "fragmented-hard (gaps hold same-encoder media: the real test)",
        &hard,
    );

    // Every file in both fixtures must genuinely be fragmented, or its row is
    // not measuring reassembly at all.
    for r in easy.iter().chain(&hard) {
        assert!(
            r.true_fragments >= 2,
            "{} is in {} fragment(s); it would test nothing",
            r.file,
            r.true_fragments
        );
        assert!(
            !r.contiguous_recovers,
            "{} came back whole without reassembly, so it is not really fragmented",
            r.file
        );
    }

    // The safety property, and the one worth more than any recovery count: a
    // file that did not come back byte-exactly must never be reported complete.
    // A wrong file labelled Valid is what a user would restore and keep,
    // believing it. Every JPEG here did exactly that before the entropy decoder
    // landed - insertion leaves the restart-marker sequence intact, so the
    // marker walk saw nothing wrong with 32 KiB of filler in the middle of a
    // scan.
    for r in easy.iter().chain(&hard) {
        assert!(
            !(r.status == Status::Valid && !r.recovered),
            "{} did not come back byte-exactly, but the validator calls it complete",
            r.file
        );
    }

    // What each fixture recovers, from measurement. Asserted exactly, so that a
    // regression fails and so does an improvement nobody wrote down.
    let expected: &[(&str, &[&str])] = &[
        // Every JPEG, and the MP4 whose index precedes its media.
        (
            "fragmented-jpeg",
            &["large_a.jpg", "large_b.jpg", "large_c.mp4", "large_f.jpg"],
        ),
        // Against gaps holding same-encoder media: the JPEG that restarts once
        // per MCU row, and the one with no restart markers at all - which is
        // protected instead by the DC predictor never being reset, so foreign
        // coefficients drift out of range within a cluster.
        ("fragmented-hard", &["large_b.jpg", "large_f.jpg"]),
    ];
    for ((name, want), rows) in expected.iter().zip([&easy, &hard]) {
        let got: Vec<&str> = rows
            .iter()
            .filter(|r| r.recovered)
            .map(|r| r.file.as_str())
            .collect();
        assert_eq!(
            got, *want,
            "{name}: the set of byte-exact recoveries changed. If this is an \
             improvement, update the expectation and docs/PROGRESS.md with the new \
             numbers rather than deleting the check."
        );
    }
}
