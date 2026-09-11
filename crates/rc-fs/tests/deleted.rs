//! Milestone 2 acceptance: recover deleted names, sizes, timestamps and the
//! original folder tree from the generated fixtures.
//!
//! SPEC.md section 8 sets the bar at ">=95% name accuracy". The deleted set
//! is deliberately adversarial rather than a flat directory of short ASCII
//! names - it includes nine levels of nesting, five non-ASCII names across four
//! scripts (one with surrogate pairs), a 250-character name at the format
//! limit, and a file fragmented before deletion - because a score against easy
//! names measures nothing.
//!
//! With 16 deleted files, 95% allows zero misses.

use rc_fs::{EntryState, FsType};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// fixture loading
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn fixture_dir() -> PathBuf {
    workspace_root().join("testdata").join("fixtures")
}

struct Expected {
    /// relative path -> (size, sha256)
    deleted: BTreeMap<String, (u64, String)>,
    present: BTreeMap<String, (u64, String)>,
    image_sha256: String,
    fragmented_path: Option<String>,
    fragmented_layout: Option<String>,
    /// The volume starts at a partition offset rather than at LBA 0. The
    /// ntfs-3g fixtures are bare filesystems; anything Windows formats through
    /// diskpart gets a partition table whether you asked for one or not.
    partitioned: bool,
}

/// Shared lock over the fixtures directory; see rc-device's immutability test.
fn lock_fixtures() -> Option<rc_device::testutil::FixtureLock> {
    rc_device::testutil::lock_fixtures(&fixture_dir()).ok()
}

fn load_expected(name: &str) -> Option<(PathBuf, Expected)> {
    let img = fixture_dir().join(format!("{name}.img"));
    let json = fixture_dir().join(format!("{name}.expected.json"));
    if !img.exists() || !json.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&json).expect("read expected.json");
    let v: serde_json::Value = serde_json::from_str(&text).expect("parse expected.json");

    let mut deleted = BTreeMap::new();
    let mut present = BTreeMap::new();
    for (path, meta) in v["files"].as_object().expect("files object") {
        let size = meta["size"].as_u64().unwrap_or(0);
        let sha = meta["sha256"].as_str().unwrap_or_default().to_string();
        match meta["state"].as_str() {
            Some("deleted") => {
                deleted.insert(path.clone(), (size, sha));
            }
            _ => {
                present.insert(path.clone(), (size, sha));
            }
        }
    }

    Some((
        img,
        Expected {
            deleted,
            present,
            image_sha256: v["image_sha256"].as_str().unwrap_or_default().to_string(),
            fragmented_path: v["fragmented_file"]["path"].as_str().map(|s| s.to_string()),
            fragmented_layout: v["fragmented_file"]["layout"]
                .as_str()
                .map(|s| s.to_string()),
            partitioned: v["partitioned"].as_bool().unwrap_or(false),
        },
    ))
}

fn sha256_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).expect("open image");
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    loop {
        let n = f.read(&mut buf).expect("read image");
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    hex::encode(h.finalize())
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

// ---------------------------------------------------------------------------
// the graded run
// ---------------------------------------------------------------------------

struct Score {
    fs: FsType,
    expected_deleted: usize,
    name_hits: usize,
    path_hits: usize,
    size_hits: usize,
    missing: Vec<String>,
    wrong_path: Vec<(String, String)>,
    wrong_size: Vec<(String, u64, u64)>,
    total_entries: usize,
    damaged: usize,
}

impl Score {
    fn name_accuracy(&self) -> f64 {
        if self.expected_deleted == 0 {
            return 1.0;
        }
        self.name_hits as f64 / self.expected_deleted as f64
    }
    fn path_accuracy(&self) -> f64 {
        if self.expected_deleted == 0 {
            return 1.0;
        }
        self.path_hits as f64 / self.expected_deleted as f64
    }
}

fn grade(name: &str) -> Option<Score> {
    let _lock = lock_fixtures();
    let (img, exp) = load_expected(name)?;

    let before = sha256_file(&img);
    if !exp.image_sha256.is_empty() {
        assert_eq!(
            before, exp.image_sha256,
            "{name}: the image on disk does not match its recorded ground truth; \
             rebuild the fixtures"
        );
    }

    let device = rc_device::open(&img, None).expect("open fixture");

    // A partitioned image needs the partition found first. Doing this through
    // rc-partition rather than hard-coding the offset means the graded run
    // exercises discovery too: if the table were misread the volume would not
    // be found at all, which is a louder failure than a silently wrong score.
    let offset = if exp.partitioned {
        let tbl = rc_partition::discover(device.as_ref(), &Default::default())
            .unwrap_or_else(|e| panic!("{name}: partition discovery failed: {e}"));
        let part = tbl.supported().next().unwrap_or_else(|| {
            panic!(
                "{name}: no partition holding a supported filesystem; found {:?}",
                tbl.partitions
                    .iter()
                    .map(|p| (p.index, p.start.0, p.fs))
                    .collect::<Vec<_>>()
            )
        });
        part.start.0 * device.sector_size().get() as u64
    } else {
        0
    };

    let (fs, result) = rc_fs::scan_volume(device.as_ref(), offset)
        .unwrap_or_else(|e| panic!("{name}: scan failed at offset {offset}: {e}"));

    // The source must be untouched by parsing, same invariant as Milestone 1.
    drop(device);
    assert_eq!(
        before,
        sha256_file(&img),
        "{name}: parsing modified the fixture image"
    );

    // Index recovered deleted entries by basename and by full path.
    let mut by_name: BTreeMap<String, Vec<&rc_fs::Entry>> = BTreeMap::new();
    let mut by_path: BTreeMap<String, &rc_fs::Entry> = BTreeMap::new();
    for e in result
        .entries
        .iter()
        .filter(|e| e.state == EntryState::Deleted)
    {
        by_name.entry(e.name.clone()).or_default().push(e);
        if let Some(p) = &e.path {
            by_path.insert(p.clone(), e);
        }
    }

    let mut score = Score {
        fs,
        expected_deleted: exp.deleted.len(),
        name_hits: 0,
        path_hits: 0,
        size_hits: 0,
        missing: Vec::new(),
        wrong_path: Vec::new(),
        wrong_size: Vec::new(),
        total_entries: result.entries.len(),
        damaged: result.damaged.len(),
    };

    for (path, (size, _sha)) in &exp.deleted {
        let want_name = basename(path);
        match by_name.get(want_name) {
            None => {
                score.missing.push(path.clone());
                continue;
            }
            Some(candidates) => {
                score.name_hits += 1;

                if by_path.contains_key(path) {
                    score.path_hits += 1;
                } else {
                    let got = candidates
                        .iter()
                        .filter_map(|c| c.path.clone())
                        .next()
                        .unwrap_or_else(|| format!("?/{want_name}"));
                    score.wrong_path.push((path.clone(), got));
                }

                // Match the size against whichever candidate shares the name.
                let matched = candidates.iter().any(|c| c.size == *size);
                if matched {
                    score.size_hits += 1;
                } else {
                    score
                        .wrong_size
                        .push((path.clone(), *size, candidates[0].size));
                }
            }
        }
    }

    // The fragmented file must not be reported as an exact contiguous layout
    // when the filesystem cannot know that.
    if let (Some(fp), Some("fragmented")) = (&exp.fragmented_path, exp.fragmented_layout.as_deref())
    {
        if let Some(e) = by_path.get(fp.as_str()) {
            let claims_exact = e.location.is_exact() && e.location.extent_count() == 1;
            assert!(
                !claims_exact || fs == FsType::ExFat,
                "{name}: {fp} was fragmented on disk but the parser reports a single \
                 contiguous run, which would recover the wrong bytes"
            );
        }
    }

    Some(score)
}

fn report(name: &str, s: &Score) {
    eprintln!("\n=== {name} ({}) ===", s.fs);
    eprintln!(
        "  entries recovered : {} ({} damaged records)",
        s.total_entries, s.damaged
    );
    eprintln!(
        "  name accuracy     : {}/{} = {:.1}%",
        s.name_hits,
        s.expected_deleted,
        s.name_accuracy() * 100.0
    );
    eprintln!(
        "  path accuracy     : {}/{} = {:.1}%",
        s.path_hits,
        s.expected_deleted,
        s.path_accuracy() * 100.0
    );
    eprintln!(
        "  size accuracy     : {}/{}",
        s.size_hits, s.expected_deleted
    );
    for m in &s.missing {
        eprintln!("  MISSING  {m}");
    }
    for (want, got) in &s.wrong_path {
        eprintln!("  PATH     want {want}\n           got  {got}");
    }
    for (p, want, got) in &s.wrong_size {
        eprintln!("  SIZE     {p}: want {want}, got {got}");
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

const BAR: f64 = 0.95;

/// Report the toolchain each fixture was built with.
///
/// The corpus generator is byte-for-byte deterministic on one toolchain and
/// not across toolchains - SQLite stamps its library version into every
/// database header, and DEFLATE output varies between zlib versions, which
/// between them makes 58 of the 315 files differ in content at identical size.
/// So the toolchain is part of the ground truth, and a fixture that does not
/// record it cannot have its per-file hashes re-derived later.
///
/// This reports rather than fails: a fixture built on another toolchain is
/// perfectly valid for itself, and the cross-toolchain diversity is the point
/// of having a Windows-built NTFS image at all. The loud failure lives where
/// it is actionable - the ground-truth writers refuse to re-describe an
/// unchanged image with a manifest from a different toolchain.
#[test]
fn fixtures_record_the_toolchain_that_built_them() {
    let names = [
        "ntfs-basic",
        "ntfs-windows",
        "fat32-basic",
        "exfat-basic",
        "ext4-basic",
        "quickformat",
        "overwritten",
        "nopart",
        "fragmented-jpeg",
    ];
    let mut without = Vec::new();
    let mut found = 0usize;

    eprintln!("\nfixture provenance:");
    for name in names {
        let json = fixture_dir().join(format!("{name}.expected.json"));
        if !json.exists() {
            continue;
        }
        found += 1;
        let text = std::fs::read_to_string(&json).expect("read expected.json");
        let v: serde_json::Value = serde_json::from_str(&text).expect("parse expected.json");
        let prov = &v["generator"]["provenance"];
        if prov.is_object() {
            eprintln!(
                "  {name:<17} {} / sqlite {} / zlib {}",
                prov["system"].as_str().unwrap_or("?"),
                prov["sqlite_library"].as_str().unwrap_or("?"),
                prov["zlib"].as_str().unwrap_or("?"),
            );
        } else {
            eprintln!("  {name:<17} NONE RECORDED");
            without.push(name);
        }
    }

    assert!(found > 0, "no fixtures are built");
    if !without.is_empty() {
        eprintln!(
            "\n  note: {} fixture(s) predate the provenance field and cannot have their \n\
               per-file hashes re-derived: {without:?}\n  \
             Rebuilding them through testdata/build_fixtures.sh records it.",
            without.len()
        );
    }
}

#[test]
fn ntfs_recovers_deleted_entries() {
    let Some(s) = grade("ntfs-basic") else {
        eprintln!("note: ntfs-basic fixture not built; run testdata/build_fixtures.sh");
        return;
    };
    report("ntfs-basic", &s);
    assert!(
        s.name_accuracy() >= BAR,
        "NTFS name accuracy {:.1}% is below the {:.0}% bar",
        s.name_accuracy() * 100.0,
        BAR * 100.0
    );
}

/// The same corpus, written by a different NTFS implementation.
///
/// Every other NTFS fixture is formatted and populated by ntfs-3g through one
/// script. However large that corpus grows it stays a single sample: a
/// systematic difference that Microsoft's driver produces and ntfs-3g never
/// does - an $ATTRIBUTE_LIST layout, an index-allocation pattern, a
/// resident-attribute threshold - cannot be surfaced by it at any size,
/// because the errors are correlated. This image was formatted and written by
/// the Windows NTFS driver instead. See docs/LIMITATIONS.md section 2.7.
#[test]
fn ntfs_from_the_microsoft_driver_recovers_deleted_entries() {
    let Some(s) = grade("ntfs-windows") else {
        eprintln!(
            "note: ntfs-windows fixture not built; run scripts/make-windows-fixture.ps1 \
             from an Administrator PowerShell on Windows"
        );
        return;
    };
    report("ntfs-windows", &s);
    assert!(
        s.name_accuracy() >= BAR,
        "NTFS (Microsoft driver) name accuracy {:.1}% is below the {:.0}% bar",
        s.name_accuracy() * 100.0,
        BAR * 100.0
    );
}

/// Compare the two NTFS samples against each other.
///
/// Agreement is reassuring. Disagreement is the more valuable result, because
/// it is a finding about the parser rather than about the fixture generator -
/// so this reports what differs rather than only whether both cleared the bar.
///
/// Note what cannot be compared: LIMITATIONS section 2.7 records that 58 of the
/// 315 corpus files differ in content between the two images, because their
/// sqlite files were written by different SQLite versions and their docx
/// members deflated by different zlib versions. Names, paths and sizes are
/// identical across both, and those are what this test compares.
#[test]
fn the_two_ntfs_drivers_agree_on_what_was_deleted() {
    let (Some(a), Some(b)) = (grade("ntfs-basic"), grade("ntfs-windows")) else {
        eprintln!("note: both NTFS fixtures are needed for the cross-driver comparison");
        return;
    };

    eprintln!(
        "\ncross-driver comparison\n  ntfs-3g  : name {:.1}%  path {:.1}%  size {}/{}\n  \
         Microsoft: name {:.1}%  path {:.1}%  size {}/{}",
        a.name_accuracy() * 100.0,
        a.path_accuracy() * 100.0,
        a.size_hits,
        a.expected_deleted,
        b.name_accuracy() * 100.0,
        b.path_accuracy() * 100.0,
        b.size_hits,
        b.expected_deleted,
    );

    // The total entry counts differ by an order of magnitude and that is
    // expected, not a discrepancy to explain away: build_fixtures.sh forces
    // fragmentation by writing thousands of filler files and deleting them, so
    // ntfs-basic carries all of those as deleted MFT records. The Windows
    // script does not fragment, so its count is just the corpus plus metadata -
    // 315 files, 23 directories and 14 NTFS metafiles.
    //
    // Files one driver's image gave up and the other's did not. Either
    // direction is a genuine finding: it means the parser depends on a layout
    // choice rather than on the format.
    let only_in_basic: Vec<&String> = b
        .missing
        .iter()
        .filter(|m| !a.missing.contains(m))
        .collect();
    let only_in_windows: Vec<&String> = a
        .missing
        .iter()
        .filter(|m| !b.missing.contains(m))
        .collect();
    for m in &only_in_basic {
        eprintln!("  RECOVERED FROM ntfs-3g BUT NOT FROM THE MICROSOFT DRIVER: {m}");
    }
    for m in &only_in_windows {
        eprintln!("  RECOVERED FROM THE MICROSOFT DRIVER BUT NOT FROM ntfs-3g: {m}");
    }

    assert_eq!(
        a.expected_deleted, b.expected_deleted,
        "the two fixtures do not describe the same number of deleted files"
    );
    assert!(
        only_in_basic.is_empty() && only_in_windows.is_empty(),
        "the parser disagrees between NTFS implementations, which is a parser bug \
         rather than a fixture one: {} file(s) recoverable from only one of them",
        only_in_basic.len() + only_in_windows.len()
    );
}

#[test]
fn fat32_recovers_deleted_entries() {
    let Some(s) = grade("fat32-basic") else {
        eprintln!("note: fat32-basic fixture not built");
        return;
    };
    report("fat32-basic", &s);
    assert!(
        s.name_accuracy() >= BAR,
        "FAT32 name accuracy {:.1}% is below the {:.0}% bar. The long names live \
         in the LFN chain, which survives deletion.",
        s.name_accuracy() * 100.0,
        BAR * 100.0
    );
}

#[test]
fn exfat_recovers_deleted_entries() {
    let Some(s) = grade("exfat-basic") else {
        eprintln!("note: exfat-basic fixture not built");
        return;
    };
    report("exfat-basic", &s);
    assert!(
        s.name_accuracy() >= BAR,
        "exFAT name accuracy {:.1}% is below the {:.0}% bar. Deletion only clears \
         the in-use bit, so nothing about the name should be lost.",
        s.name_accuracy() * 100.0,
        BAR * 100.0
    );
}

/// The folder tree must be rebuilt, not just the names.
#[test]
fn original_folder_tree_is_reconstructed() {
    for name in ["ntfs-basic", "fat32-basic", "exfat-basic"] {
        let Some(s) = grade(name) else { continue };
        assert!(
            s.path_accuracy() >= BAR,
            "{name}: path accuracy {:.1}% is below the {:.0}% bar; \
             {} deleted files were placed in the wrong directory",
            s.path_accuracy() * 100.0,
            BAR * 100.0,
            s.wrong_path.len()
        );
    }
}

/// Sizes come from the same metadata as the names and should be exact.
#[test]
fn sizes_are_recovered() {
    for name in ["ntfs-basic", "fat32-basic", "exfat-basic"] {
        let Some(s) = grade(name) else { continue };
        assert!(
            s.size_hits as f64 / s.expected_deleted.max(1) as f64 >= BAR,
            "{name}: only {}/{} deleted files had the right size",
            s.size_hits,
            s.expected_deleted
        );
    }
}

/// ext4 is Milestone 6 and must say so rather than returning nothing.
#[test]
fn ext4_reports_that_it_is_unimplemented() {
    let Some((img, _)) = load_expected("ext4-basic") else {
        return;
    };
    let device = rc_device::open(&img, None).expect("open");
    match rc_fs::scan_volume(device.as_ref(), 0) {
        Err(rc_fs::FsError::Unimplemented { fs }) => {
            assert!(
                fs.contains("ext4"),
                "the message should name the filesystem"
            );
        }
        Err(e) => panic!("expected Unimplemented, got {e}"),
        Ok(_) => panic!("ext4 must not silently claim to work"),
    }
}

/// Detection must not depend on a partition type code.
#[test]
fn detects_each_filesystem_from_its_boot_sector() {
    for (name, want) in [
        ("ntfs-basic", FsType::Ntfs),
        ("fat32-basic", FsType::Fat32),
        ("exfat-basic", FsType::ExFat),
        ("ext4-basic", FsType::Ext4),
    ] {
        let Some((img, _)) = load_expected(name) else {
            continue;
        };
        let device = rc_device::open(&img, None).expect("open");
        let got = rc_fs::detect(device.as_ref(), 0).expect("detect");
        assert_eq!(got, want, "{name} was detected as {got}");
    }
}

/// Allocated files must be enumerated too, so the scoring engine knows which
/// clusters are occupied (SPEC.md section 5.4).
#[test]
fn allocated_files_are_enumerated_for_the_scoring_engine() {
    for name in ["ntfs-basic", "fat32-basic", "exfat-basic"] {
        let Some((img, exp)) = load_expected(name) else {
            continue;
        };
        let device = rc_device::open(&img, None).expect("open");
        let (_, result) = rc_fs::scan_volume(device.as_ref(), 0).expect("scan");
        let allocated = result.allocated().count();
        assert!(
            allocated >= exp.present.len(),
            "{name}: {allocated} allocated entries found but {} files are still present",
            exp.present.len()
        );
    }
}
