//! `rc verify` - check an image against its `.hash` manifest.

use crate::output::{human_bytes, print_json};
use clap::Args as ClapArgs;
use std::path::PathBuf;

#[derive(ClapArgs)]
pub struct Args {
    /// Image to verify.
    image: PathBuf,

    /// Manifest path. Defaults to <image>.hash.
    #[arg(long)]
    manifest: Option<PathBuf>,
}

#[derive(serde::Serialize)]
struct VerifyJson {
    image: String,
    manifest: String,
    passed: bool,
    sha256_ok: bool,
    md5_ok: bool,
    size_ok: bool,
    expected_sha256: String,
    actual_sha256: String,
    expected_md5: String,
    actual_md5: String,
    expected_bytes: u64,
    actual_bytes: u64,
    source_was_incomplete: bool,
}

pub fn run(args: Args, json: bool) -> anyhow::Result<()> {
    let manifest = args
        .manifest
        .clone()
        .unwrap_or_else(|| rc_image::hash_path_for(&args.image));

    if !manifest.exists() {
        anyhow::bail!(
            "no manifest at {}. Pass --manifest to point at one.",
            manifest.display()
        );
    }

    let report = rc_image::verify_against_manifest(&args.image, &manifest)?;

    if json {
        print_json(&VerifyJson {
            image: args.image.to_string_lossy().to_string(),
            manifest: manifest.to_string_lossy().to_string(),
            passed: report.passed(),
            sha256_ok: report.sha256_ok,
            md5_ok: report.md5_ok,
            size_ok: report.size_ok,
            expected_sha256: report.expected.sha256.clone(),
            actual_sha256: report.actual.sha256.clone(),
            expected_md5: report.expected.md5.clone(),
            actual_md5: report.actual.md5.clone(),
            expected_bytes: report.expected.bytes,
            actual_bytes: report.actual.bytes,
            source_was_incomplete: report.source_was_incomplete,
        })?;
    } else {
        println!("image:    {}", args.image.display());
        println!("manifest: {}", manifest.display());
        println!();
        println!(
            "size    {}  expected {}  actual {}",
            mark(report.size_ok),
            human_bytes(report.expected.bytes),
            human_bytes(report.actual.bytes)
        );
        println!(
            "sha256  {}  {}",
            mark(report.sha256_ok),
            report.actual.sha256
        );
        if !report.sha256_ok {
            println!("        expected {}", report.expected.sha256);
        }
        println!("md5     {}  {}", mark(report.md5_ok), report.actual.md5);
        if !report.md5_ok {
            println!("        expected {}", report.expected.md5);
        }
        println!();
        if report.passed() {
            println!("VERIFIED: the image matches its manifest.");
        } else {
            println!("FAILED: the image does not match its manifest.");
        }
        if report.source_was_incomplete {
            println!(
                "note: the manifest records an incomplete clone (the source had \
                 unreadable regions), so this image is not a faithful copy of the \
                 original device even when it verifies against the manifest."
            );
        }
    }

    if !report.passed() {
        std::process::exit(1);
    }
    Ok(())
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "OK  "
    } else {
        "FAIL"
    }
}
