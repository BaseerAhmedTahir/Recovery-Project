//! Cloning integration tests.
//!
//! Milestone 1 acceptance (SPEC.md section 8): "clone a fixture to `.raw`
//! with bad-sector skipping and matching SHA-256; source hash unchanged."
//!
//! Bad sectors are produced with `rc_device::testutil::FaultyDevice` rather
//! than a physically failing drive. See `docs/LIMITATIONS.md` section 4 for
//! what that does and does not prove.

use rc_device::testutil::FaultyDevice;
use rc_image::{
    clone_device, hash_file, hash_path_for, map_path_for, verify_against_manifest, BlockMap,
    BlockStatus, CloneOptions, RescueOptions, SinkOptions,
};
use std::io::Write;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join("rc-image-clone-tests").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// An image whose every sector is identifiable, so a misplaced write is
/// obvious rather than subtle.
fn make_image(path: &Path, sectors: u64) {
    let mut f = std::fs::File::create(path).unwrap();
    for lba in 0..sectors {
        let mut s = [0u8; 512];
        s[..8].copy_from_slice(&lba.to_le_bytes());
        for (i, b) in s.iter_mut().enumerate().skip(8) {
            *b = ((lba as usize).wrapping_mul(37).wrapping_add(i) % 251) as u8;
        }
        f.write_all(&s).unwrap();
    }
    f.sync_all().unwrap();
}

/// An image with long zero runs, to exercise sparse output.
fn make_sparse_image(path: &Path, sectors: u64) {
    let mut f = std::fs::File::create(path).unwrap();
    for lba in 0..sectors {
        let mut s = [0u8; 512];
        if lba % 64 == 0 {
            s[..8].copy_from_slice(&lba.to_le_bytes());
            s[8] = 0xAB;
        }
        f.write_all(&s).unwrap();
    }
    f.sync_all().unwrap();
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .to_path_buf()
}

fn first_fixture() -> Option<PathBuf> {
    let dir = workspace_root().join("testdata").join("fixtures");
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("img"))
        .collect();
    v.sort();
    v.into_iter().next()
}

// ---------------------------------------------------------------------------
// the acceptance test
// ---------------------------------------------------------------------------

/// A clean clone must be byte-identical, and must not touch the source.
#[test]
fn clone_is_byte_exact_and_leaves_the_source_unchanged() {
    let d = tmpdir("exact");
    let src = d.join("source.img");
    make_image(&src, 4096); // 2 MiB
    let src_hash_before = hash_file(&src).unwrap();

    let out = d.join("clone.raw");
    let report = {
        let dev = rc_device::open(&src, None).unwrap();
        clone_device(dev.as_ref(), &out, &CloneOptions::default(), None).unwrap()
    };

    assert!(report.complete, "clone should be complete");
    assert_eq!(report.bad_bytes, 0);

    let src_hash_after = hash_file(&src).unwrap();
    assert_eq!(
        src_hash_before, src_hash_after,
        "cloning modified the source image"
    );

    let out_hash = hash_file(&out).unwrap();
    assert_eq!(
        out_hash.sha256, src_hash_before.sha256,
        "clone is not byte-identical to the source"
    );
    assert_eq!(out_hash.md5, src_hash_before.md5);
    assert_eq!(out_hash.bytes, src_hash_before.bytes);

    // The streaming digest computed during the copy must agree with a fresh
    // read of the source; otherwise the .hash manifest is meaningless.
    assert_eq!(
        report.source_digests.sha256, src_hash_before.sha256,
        "streaming digest disagrees with a full re-read of the source"
    );

    // Sidecars exist and are consistent.
    assert!(report.map_path.exists());
    assert!(report.hash_path.exists());
    let v = verify_against_manifest(&out, &report.hash_path).unwrap();
    assert!(v.passed(), "verify against the manifest failed: {v:?}");
    assert!(!v.source_was_incomplete);
}

/// The same, against a real generated fixture when one is available.
#[test]
fn clones_a_generated_fixture_byte_exactly() {
    let Some(fixture) = first_fixture() else {
        eprintln!("note: no fixtures built; run testdata/build_fixtures.sh");
        return;
    };
    let d = tmpdir("fixture");
    let out = d.join("fixture-clone.raw");

    let before = hash_file(&fixture).unwrap();
    let report = {
        let dev = rc_device::open(&fixture, None).unwrap();
        clone_device(dev.as_ref(), &out, &CloneOptions::default(), None).unwrap()
    };
    let after = hash_file(&fixture).unwrap();

    assert_eq!(before, after, "cloning modified the fixture");
    assert!(report.complete);
    assert_eq!(
        hash_file(&out).unwrap().sha256,
        before.sha256,
        "fixture clone is not byte-exact"
    );
}

// ---------------------------------------------------------------------------
// bad sectors
// ---------------------------------------------------------------------------

/// Unreadable regions must be skipped, recorded in the map, and reported
/// honestly rather than silently filled and declared complete.
#[test]
fn bad_sectors_are_skipped_recorded_and_reported() {
    let d = tmpdir("badsectors");
    let src = d.join("source.img");
    make_image(&src, 4096); // 2 MiB
    let before = hash_file(&src).unwrap();

    // Two damaged regions, both away from the edges.
    let bad = vec![100 * 512..108 * 512, 2000 * 512..2004 * 512];
    let bad_bytes: u64 = bad.iter().map(|r| r.end - r.start).sum();

    let out = d.join("clone.raw");
    let report = {
        let inner = rc_device::open(&src, None).unwrap();
        let faulty = FaultyDevice::new(inner, bad.clone());
        clone_device(
            &faulty,
            &out,
            &CloneOptions {
                rescue: RescueOptions {
                    block_bytes: 64 * 1024,
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        )
        .unwrap()
    };

    assert_eq!(
        hash_file(&src).unwrap(),
        before,
        "a failing clone must still not modify the source"
    );

    assert!(
        !report.complete,
        "clone with bad sectors must not be complete"
    );
    assert_eq!(
        report.bad_bytes, bad_bytes,
        "the map should mark exactly the injected bad bytes"
    );

    // The map must localise the damage rather than condemning whole blocks:
    // that is the entire point of the trim and scrape passes.
    let map = BlockMap::read_from(&report.map_path).unwrap();
    assert_eq!(map.bytes_in(BlockStatus::Bad), bad_bytes);
    assert_eq!(
        map.bytes_in(BlockStatus::Finished),
        map.size() - bad_bytes,
        "everything outside the bad ranges should have been recovered"
    );

    // Good data on both sides of a bad region must be correct, not shifted.
    let good_src = std::fs::read(&src).unwrap();
    let good_out = std::fs::read(&out).unwrap();
    assert_eq!(good_out.len(), good_src.len());
    assert_eq!(&good_out[0..512], &good_src[0..512], "start of device");
    assert_eq!(
        &good_out[99 * 512..100 * 512],
        &good_src[99 * 512..100 * 512],
        "sector immediately before a bad region"
    );
    assert_eq!(
        &good_out[108 * 512..109 * 512],
        &good_src[108 * 512..109 * 512],
        "sector immediately after a bad region"
    );
    assert_eq!(
        &good_out[4095 * 512..],
        &good_src[4095 * 512..],
        "end of device"
    );

    // The manifest must say the clone is incomplete.
    let manifest = rc_image::HashManifest::read_from(&report.hash_path).unwrap();
    assert!(!manifest.complete);
    assert_eq!(manifest.bad_bytes, bad_bytes);
}

/// A marginal sector that reads on a later attempt must be recovered by the
/// retry pass, and the map must be updated to match.
#[test]
fn retry_pass_recovers_flaky_sectors() {
    let d = tmpdir("flaky");
    let src = d.join("source.img");
    make_image(&src, 1024);

    let out = d.join("clone.raw");
    let report = {
        let inner = rc_device::open(&src, None).unwrap();
        // Fails the copy, trim and scrape attempts, then starts succeeding.
        let faulty = FaultyDevice::new(
            inner,
            vec![std::ops::Range {
                start: 500 * 512,
                end: 502 * 512,
            }],
        )
        .flaky_after(4);
        clone_device(
            &faulty,
            &out,
            &CloneOptions {
                rescue: RescueOptions {
                    block_bytes: 64 * 1024,
                    retries: 2,
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        )
        .unwrap()
    };

    assert_eq!(
        report.bad_bytes, 0,
        "the retry pass should have recovered the flaky sectors"
    );
    assert!(report.complete);
    assert_eq!(
        hash_file(&out).unwrap().sha256,
        hash_file(&src).unwrap().sha256,
        "a fully retried clone should still be byte-exact"
    );
}

// ---------------------------------------------------------------------------
// resume, sparse, safety
// ---------------------------------------------------------------------------

/// Resuming must not redo finished work and must reach the same result.
#[test]
fn resume_continues_from_the_map() {
    let d = tmpdir("resume");
    let src = d.join("source.img");
    make_image(&src, 2048);
    let out = d.join("clone.raw");

    // First run: pretend the first half is already done.
    {
        let dev = rc_device::open(&src, None).unwrap();
        let total = dev.total_bytes();
        let mut map = BlockMap::new(total);
        map.mark(0, total / 2, BlockStatus::Finished);
        map.write_to(&map_path_for(&out)).unwrap();
    }
    // The output must already contain the first half for resume to be correct.
    {
        let data = std::fs::read(&src).unwrap();
        let mut f = std::fs::File::create(&out).unwrap();
        f.write_all(&data[..data.len() / 2]).unwrap();
        f.set_len(data.len() as u64).unwrap();
    }

    let report = {
        let dev = rc_device::open(&src, None).unwrap();
        clone_device(
            dev.as_ref(),
            &out,
            &CloneOptions {
                resume: true,
                sink: SinkOptions {
                    allow_overwrite: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        )
        .unwrap()
    };

    assert!(report.complete);
    assert_eq!(
        hash_file(&out).unwrap().sha256,
        hash_file(&src).unwrap().sha256,
        "a resumed clone should still be byte-exact"
    );
    // Only the unfinished half should have been copied.
    assert!(
        report.stats.bytes_copied <= 2048 * 512 / 2 + 512,
        "resume re-copied already-finished ranges: {} bytes",
        report.stats.bytes_copied
    );
}

/// Resuming against a differently-sized device must be refused, not silently
/// produce a corrupt image.
#[test]
fn resume_refuses_a_mismatched_device() {
    let d = tmpdir("mismatch");
    let src = d.join("source.img");
    make_image(&src, 1024);
    let out = d.join("clone.raw");

    BlockMap::new(999 * 512)
        .write_to(&map_path_for(&out))
        .unwrap();

    let dev = rc_device::open(&src, None).unwrap();
    let err = clone_device(
        dev.as_ref(),
        &out,
        &CloneOptions {
            resume: true,
            ..Default::default()
        },
        None,
    )
    .expect_err("resume against a different size must fail");
    assert!(matches!(err, rc_image::ImageError::ResumeMismatch { .. }));
}

/// A sparse clone must still be byte-exact when read back.
#[test]
fn sparse_output_is_still_byte_exact() {
    let d = tmpdir("sparse");
    let src = d.join("source.img");
    make_sparse_image(&src, 4096);
    let out = d.join("clone.raw");

    let report = {
        let dev = rc_device::open(&src, None).unwrap();
        clone_device(dev.as_ref(), &out, &CloneOptions::default(), None).unwrap()
    };

    assert!(report.complete);
    assert_eq!(
        hash_file(&out).unwrap().sha256,
        hash_file(&src).unwrap().sha256,
        "sparse clone differs from the source"
    );
    assert_eq!(
        std::fs::metadata(&out).unwrap().len(),
        std::fs::metadata(&src).unwrap().len(),
        "sparse clone has the wrong apparent size"
    );
}

/// The cross-crate safety invariant, at the clone level rather than the sink
/// level: cloning an image onto itself must be refused.
#[test]
fn refuses_to_clone_an_image_onto_itself() {
    let d = tmpdir("selfclone");
    let src = d.join("source.img");
    make_image(&src, 256);
    let before = hash_file(&src).unwrap();

    let dev = rc_device::open(&src, None).unwrap();
    let err = clone_device(
        dev.as_ref(),
        &src,
        &CloneOptions {
            sink: SinkOptions {
                allow_overwrite: true,
                ..Default::default()
            },
            ..Default::default()
        },
        None,
    )
    .expect_err("cloning an image onto itself must be refused");

    assert!(
        matches!(err, rc_image::ImageError::WouldWriteToSource { .. }),
        "expected WouldWriteToSource, got {err:?}"
    );
    assert_eq!(
        hash_file(&src).unwrap(),
        before,
        "the refused clone must have left the source untouched"
    );
}

/// Sidecar naming, so `rc verify` can find the manifest without being told.
#[test]
fn sidecars_are_named_predictably() {
    let d = tmpdir("sidecars");
    let src = d.join("source.img");
    make_image(&src, 128);
    let out = d.join("clone.raw");

    let dev = rc_device::open(&src, None).unwrap();
    let report = clone_device(dev.as_ref(), &out, &CloneOptions::default(), None).unwrap();

    assert_eq!(report.map_path, map_path_for(&out));
    assert_eq!(report.hash_path, hash_path_for(&out));
    assert_eq!(report.map_path.file_name().unwrap(), "clone.raw.map");
    assert_eq!(report.hash_path.file_name().unwrap(), "clone.raw.hash");
}
