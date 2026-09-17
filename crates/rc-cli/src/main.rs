//! `rc` - the RECOVERY-CORE command line.
//!
//! SPEC.md section 0: "The CLI is the source of truth; the GUI is a thin
//! client over the same library." Every capability lands here first.
//!
//! Milestone 1 shipped `devices`, `image`, `verify` and the opt-in `smoke`
//! check. Milestone 2 added `list-deleted`. Milestone 3 adds `carve`.

mod cmd;
mod output;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rc",
    version,
    about = "Offline data recovery engine",
    long_about = "RECOVERY-CORE: an offline, read-only data recovery engine.\n\n\
                  Devices are never opened for writing. Output always goes \
                  through a sink that refuses any destination resolving to a \
                  device currently being scanned."
)]
struct Cli {
    /// Emit machine-readable JSON instead of a table.
    #[arg(long, global = true)]
    json: bool,

    /// Increase log verbosity (repeatable).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List block devices and image files visible to this machine.
    ///
    /// Enumeration reads metadata only and never touches sector data, so it
    /// works without elevation. Devices whose data cannot be read without
    /// elevation are still listed, with the reason shown.
    Devices(cmd::devices::Args),

    /// Clone a device or image to a .raw file with ddrescue-style recovery.
    Image(cmd::image::Args),

    /// Verify an image against its .hash manifest.
    Verify(cmd::verify::Args),

    /// Recover deleted filenames, sizes, timestamps and the original folder
    /// tree from a device or image.
    ///
    /// Supports NTFS, FAT12/16/32 and exFAT. Allocated files are hidden by
    /// default; pass --include-allocated to see them.
    #[command(name = "list-deleted")]
    ListDeleted(cmd::list_deleted::Args),

    /// Carve files out of raw sectors by signature, without needing a
    /// filesystem.
    ///
    /// Reports candidates per GB scanned alongside the recovered count,
    /// because a carve is judged by its denominator: a run that emits eighty
    /// thousand candidates and happens to include the files you wanted has
    /// perfect recall and no value.
    Carve(cmd::carve::Args),

    /// Rate deleted files GREEN, YELLOW or RED, with the reasons for each.
    Score(cmd::score::Args),

    /// Render a byte range as a PNG preview (image thumbnail or video frame),
    /// decoded in memory with no temp files.
    Preview(cmd::preview::PreviewArgs),

    /// Print a byte range as annotated hex.
    Hex(cmd::preview::HexArgs),

    /// Read a few sectors from a real device to prove the unbuffered read path
    /// works on this hardware. Opt-in, never run by CI.
    Smoke(cmd::smoke::Args),
}

fn main() {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let result = match cli.command {
        Command::Devices(a) => cmd::devices::run(a, cli.json),
        Command::Image(a) => cmd::image::run(a, cli.json),
        Command::Verify(a) => cmd::verify::run(a, cli.json),
        Command::ListDeleted(a) => cmd::list_deleted::run(a, cli.json),
        Command::Carve(a) => cmd::carve::run(a, cli.json),
        Command::Score(a) => cmd::score::run(a, cli.json),
        Command::Preview(a) => cmd::preview::preview(a, cli.json),
        Command::Hex(a) => cmd::preview::hex(a, cli.json),
        Command::Smoke(a) => cmd::smoke::run(a, cli.json),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        let mut source = std::error::Error::source(&*e);
        while let Some(s) = source {
            eprintln!("  caused by: {s}");
            source = std::error::Error::source(s);
        }
        std::process::exit(1);
    }
}

fn init_tracing(verbosity: u8) {
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = std::env::var("RC_LOG").unwrap_or_else(|_| format!("rc_={level},{level}"));
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

/// Shared helper: parse an optional explicit sector size.
pub fn parse_sector_size(v: Option<u32>) -> anyhow::Result<Option<rc_device::SectorSize>> {
    match v {
        None => Ok(None),
        Some(n) => Ok(Some(rc_device::SectorSize::new(n)?)),
    }
}
