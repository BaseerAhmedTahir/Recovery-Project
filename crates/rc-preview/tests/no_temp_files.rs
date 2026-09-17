//! Milestone 5 acceptance: "previews render without temp files".
//!
//! Real media from foreign encoders (JPEG and PNG from ImageMagick, H.264 MP4s
//! from ffmpeg in both layouts: moov first, and moov last as recorders write it)
//! is previewed in memory and through ffmpeg over pipes. Before and after, the
//! system temp directory and ffmpeg's own working directory are listed; nothing
//! may appear in either. One test in its own binary, so no other test's temp
//! files can confuse the listing.

use rc_preview::{thumbnail, video_frame, FfmpegOptions, PreviewError};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("testdata")
        .join("corpus")
}

/// Bytes from a make_corpus.py generator, by Python expression.
fn generate(expr: &str) -> Vec<u8> {
    let script = format!(
        "import sys; sys.path.insert(0, sys.argv[1]); import make_corpus as m; \
         sys.stdout.buffer.write({expr})"
    );
    for py in ["python3", "python"] {
        if let Ok(out) = Command::new(py)
            .arg("-c")
            .arg(&script)
            .arg(corpus_dir())
            .output()
        {
            assert!(
                out.status.success(),
                "{expr}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            return out.stdout;
        }
    }
    panic!("Python is required and was not found");
}

fn listing(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn png_size(png: &[u8]) -> (u32, u32) {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "output is not a PNG");
    (
        u32::from_be_bytes(png[16..20].try_into().unwrap()),
        u32::from_be_bytes(png[20..24].try_into().unwrap()),
    )
}

#[test]
fn previews_render_without_writing_any_file() {
    // Generate inputs first: the generators themselves use temp directories.
    let jpeg = generate("m.im_jpeg(1024, 768, 31, 4)");
    let png = generate("m.im_png(480, 360, 32)");
    let mp4_fast = generate("m.ff_mp4(33, faststart=True, seconds=3)");
    let mp4_last = generate("m.ff_mp4(34, faststart=False, seconds=3)");
    let ffmpeg = FfmpegOptions::find().expect("ffmpeg is required for video previews");

    let temp = std::env::temp_dir();
    let workdir = temp.join(format!("rc-preview-cwd-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).unwrap();
    let before = listing(&temp);

    let t = thumbnail(&jpeg, 256).expect("jpeg thumbnail");
    assert_eq!((t.source_width, t.source_height), (1024, 768));
    assert_eq!(png_size(&t.png), (t.width, t.height));
    assert!(t.width <= 256 && t.height <= 256);

    let t = thumbnail(&png, 256).expect("png thumbnail");
    assert_eq!((t.source_width, t.source_height), (480, 360));
    assert!(t.width <= 256 && t.height <= 256);

    let opts = FfmpegOptions {
        working_dir: Some(workdir.clone()),
        max_dim: 160,
        ..ffmpeg.clone()
    };
    for (name, mp4) in [("moov first", &mp4_fast), ("moov last", &mp4_last)] {
        let frame = video_frame(mp4, &opts).unwrap_or_else(|e| panic!("{name}: {e}"));
        let (w, h) = png_size(&frame);
        assert!(w <= 160 && h <= 160 && w > 0 && h > 0, "{name}: {w}x{h}");
    }

    // Garbage is refused, not hung on.
    let err = video_frame(&[0x5Au8; 65536], &opts).expect_err("garbage is not video");
    assert!(matches!(err, PreviewError::Ffmpeg(_)), "{err}");

    // A hard timeout kills it.
    let tight = FfmpegOptions {
        timeout: Duration::from_millis(1),
        ..opts.clone()
    };
    assert!(matches!(
        video_frame(&mp4_last, &tight),
        Err(PreviewError::Timeout(_))
    ));

    assert!(
        listing(&workdir).is_empty(),
        "ffmpeg wrote into its working directory: {:?}",
        listing(&workdir)
    );
    let after = listing(&temp);
    let new: Vec<_> = after.difference(&before).collect();
    assert!(
        new.is_empty(),
        "files appeared in the temp directory: {new:?}"
    );
    let _ = std::fs::remove_dir(&workdir);
}
