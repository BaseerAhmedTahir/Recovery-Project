//! `rc preview` and `rc hex` - look at recovered bytes without writing temp
//! files (SPEC.md 5.9).
//!
//! `rc preview` decodes a byte range in memory - an image to a thumbnail, a
//! video to one frame through ffmpeg over pipes - and writes the PNG only to the
//! `--out` path you name, after checking that path is not on the device being
//! read. `rc hex` prints a range with annotations.

use crate::output::print_json;
use clap::Args as ClapArgs;
use rc_preview::{hex_view, thumbnail, video_frame, FfmpegOptions, Span, SpanKind};
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct PreviewArgs {
    /// Device or image file (opened read-only).
    source: PathBuf,
    /// Byte offset of the file.
    #[arg(long)]
    offset: u64,
    /// Length of the file in bytes.
    #[arg(long)]
    length: u64,
    /// Where to write the PNG preview.
    #[arg(long)]
    out: PathBuf,
    /// Treat the bytes as video and take a frame with ffmpeg.
    #[arg(long)]
    video: bool,
    /// Largest thumbnail dimension.
    #[arg(long, default_value_t = 320)]
    size: u32,
}

pub fn preview(args: PreviewArgs, json: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.length <= 512 << 20,
        "previews are limited to 512 MiB of input"
    );
    let dev = rc_device::open(&args.source, None)?;
    rc_image::check_destination(&args.out, rc_image::UnknownBackingPolicy::Warn)?;
    let mut bytes = vec![0u8; args.length as usize];
    let n = dev.read_bytes_at(args.offset, &mut bytes)?;
    bytes.truncate(n);

    let (png, w, h) = if args.video {
        let mut opts = FfmpegOptions::find().ok_or_else(|| {
            anyhow::anyhow!("ffmpeg was not found on PATH; video previews need it")
        })?;
        opts.max_dim = args.size;
        let png = video_frame(&bytes, &opts)?;
        let w = u32::from_be_bytes(png[16..20].try_into()?);
        let h = u32::from_be_bytes(png[20..24].try_into()?);
        (png, w, h)
    } else {
        let t = thumbnail(&bytes, args.size)?;
        (t.png, t.width, t.height)
    };
    std::fs::write(&args.out, &png)?;
    if json {
        return print_json(&serde_json::json!({
            "out": args.out.display().to_string(),
            "width": w,
            "height": h,
            "bytes": png.len(),
        }));
    }
    println!("{}x{} preview written to {}", w, h, args.out.display());
    Ok(())
}

#[derive(ClapArgs)]
pub struct HexArgs {
    /// Device or image file (opened read-only).
    source: PathBuf,
    #[arg(long)]
    offset: u64,
    #[arg(long, default_value_t = 256)]
    length: u64,
    /// Mark a header span of this many bytes at --offset.
    #[arg(long)]
    header: Option<u64>,
}

pub fn hex(args: HexArgs, json: bool) -> anyhow::Result<()> {
    let dev = rc_device::open(&args.source, None)?;
    let spans: Vec<Span> = args
        .header
        .map(|n| Span {
            start: args.offset,
            end: args.offset + n,
            kind: SpanKind::Header,
            label: "header".into(),
        })
        .into_iter()
        .collect();
    let v = hex_view(dev.as_ref(), args.offset, args.length, &spans)?;
    if json {
        return print_json(&v);
    }
    for (i, row) in v.bytes.chunks(16).enumerate() {
        let at = v.offset + (i * 16) as u64;
        let hex: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(j, b)| {
                let pos = at + j as u64;
                let marked = v.spans.iter().any(|s| pos >= s.start && pos < s.end);
                if marked {
                    format!("[{b:02x}]")
                } else {
                    format!(" {b:02x} ")
                }
            })
            .collect();
        let ascii: String = row
            .iter()
            .map(|&b| {
                if (0x20..0x7F).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!("{at:>12x}  {}  {ascii}", hex.join(""));
    }
    for s in &v.spans {
        println!("  {:?} {}..{}: {}", s.kind, s.start, s.end, s.label);
    }
    Ok(())
}
