//! Scoring against real media, damaged in a controlled way.
//!
//! The overwritten fixture is built from the basic corpus, whose MP4s carry
//! random samples and no `avcC` - a generator shortcut recorded in
//! docs/LIMITATIONS.md 3.6b. Nothing in their media can be checked, so damage to
//! them rates GREEN there. That says something true about MP4s whose codec the
//! validator does not understand, and nothing about H.264 files, which is what
//! cameras and phones write.
//!
//! So this lays an H.264 MP4 from ffmpeg - the same settings the fragmented
//! fixtures use, taken from make_corpus.py rather than restated - into a scratch
//! image as a deleted entry with exact runs, and scores it clean, with one
//! cluster of its media overwritten, and with half of them overwritten.
//!
//! Fails rather than skips when Python or ffmpeg is missing.

use rc_fs::{DataLocation, Entry, EntryKind, EntryState, Extent, Geometry, PathConfidence};
use rc_score::{classify, inputs_for_entry, Band, Context, Occupancy, Rules, Validation};
use std::path::PathBuf;
use std::process::Command;

const C: usize = 4096;
/// Where the file starts in the scratch image, in clusters.
const START: u64 = 8;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

/// An H.264 MP4 with its moov first, from make_corpus.ff_mp4.
fn ffmpeg_mp4() -> Vec<u8> {
    let corpus = workspace_root().join("testdata").join("corpus");
    let script = "import sys; sys.path.insert(0, sys.argv[1]); import make_corpus as m; \
                  sys.stdout.buffer.write(m.ff_mp4(4711, faststart=True, seconds=4))";
    for py in ["python3", "python"] {
        if let Ok(out) = Command::new(py).arg("-c").arg(script).arg(&corpus).output() {
            assert!(
                out.status.success(),
                "make_corpus.ff_mp4 failed - is ffmpeg installed? {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return out.stdout;
        }
    }
    panic!("Python is required to generate the ffmpeg sample and was not found");
}

fn scratch_image(file: &[u8]) -> (tempfile_path::TempImage, u64) {
    let clusters = file.len().div_ceil(C) as u64;
    let mut img = vec![0u8; (START as usize + clusters as usize + 8) * C];
    let at = START as usize * C;
    img[at..at + file.len()].copy_from_slice(file);
    (tempfile_path::TempImage::new(&img), clusters)
}

/// A minimal scratch file that deletes itself.
mod tempfile_path {
    use std::path::{Path, PathBuf};
    pub struct TempImage(PathBuf);
    impl TempImage {
        pub fn new(bytes: &[u8]) -> TempImage {
            let p = std::env::temp_dir().join(format!(
                "rc-score-real-media-{}-{}.img",
                std::process::id(),
                bytes.len()
            ));
            std::fs::write(&p, bytes).expect("write scratch image");
            TempImage(p)
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
        pub fn overwrite(&self, cluster: u64, fill: &[u8]) {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&self.0)
                .unwrap();
            f.seek(SeekFrom::Start(cluster * super::C as u64)).unwrap();
            f.write_all(fill).unwrap();
        }
    }
    impl Drop for TempImage {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

fn random_cluster(seed: u32) -> Vec<u8> {
    let mut x = seed | 1;
    (0..C)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn score(img: &tempfile_path::TempImage, size: u64, clusters: u64) -> rc_score::Score {
    let dev = rc_device::open(img.path(), None).expect("open scratch image");
    let entry = Entry {
        id: 1,
        name: "clip.mp4".into(),
        path: Some("DCIM/clip.mp4".into()),
        path_confidence: PathConfidence::Exact,
        kind: EntryKind::File,
        state: EntryState::Deleted,
        size,
        allocated_size: clusters * C as u64,
        timestamps: Default::default(),
        location: DataLocation::Runs(vec![Extent::new(START, clusters)]),
        parent_id: None,
        notes: vec![],
    };
    let scan = rc_fs::ScanResult {
        geometry: Geometry {
            cluster_bytes: C as u64,
            heap_offset: 0,
            first_cluster: 0,
            cluster_count: START + clusters + 8,
        },
        ..Default::default()
    };
    let occupancy = Occupancy::default();
    let rules = Rules::builtin();
    let ctx = Context::new(dev.as_ref(), &scan, &occupancy, &rules, "test");
    classify(&inputs_for_entry(&ctx, &entry).expect("inputs"), &rules)
}

#[test]
fn damage_to_a_real_h264_mp4_is_seen_and_placed() {
    let mp4 = ffmpeg_mp4();
    assert!(
        mp4.windows(4).any(|w| w == b"avcC"),
        "the sample must be H.264 with avcC, or this tests nothing"
    );
    let (img, clusters) = scratch_image(&mp4);
    let size = mp4.len() as u64;
    assert!(clusters >= 16, "need room to damage half the media");

    let clean = score(&img, size, clusters);
    assert_eq!(
        clean.inputs.validation,
        Validation::Valid,
        "{:?}",
        clean.reasons
    );
    assert_eq!(clean.band, Band::Green, "{:?}", clean.reasons);

    // One cluster in the middle of the media.
    let mid = clusters / 2;
    img.overwrite(START + mid, &random_cluster(0xC0FFEE));
    let one = score(&img, size, clusters);
    assert_eq!(one.band, Band::Yellow, "{:#?}", one.reasons);
    match &one.inputs.validation {
        Validation::Broken {
            at_cluster: Some(k),
            ..
        } => assert!(
            *k <= mid && mid - *k <= 16,
            "the break should be placed at or shortly before cluster {mid}, got {k}"
        ),
        other => panic!("expected a placed break, got {other:?}"),
    }

    // Half the clusters, all past the header. The truth is RED; the scorer can
    // confirm only the first break in a format whose validator stops there, so
    // it says YELLOW. Pinned as a known limitation (docs/LIMITATIONS.md 3.9): if
    // this starts returning RED, the limitation is gone - update the docs.
    for k in 1..=clusters / 2 {
        img.overwrite(START + k, &random_cluster(0x5EED + k as u32));
    }
    let half = score(&img, size, clusters);
    assert_eq!(
        half.band,
        Band::Yellow,
        "half the media overwritten: {:#?}",
        half.reasons
    );
    assert_eq!(
        half.inputs.lost.len(),
        1,
        "only the first break is confirmed"
    );
}
