//! `rc android`, `rc ios`, `rc host-backups` and `rc bridge`: logical recovery
//! from phones and from phone backups on this computer (SPEC.md section 6).
//!
//! Every phone operation needs the phone's holder: an Android phone must be
//! unlocked with USB debugging authorized for this computer; an iPhone must
//! trust this computer. Nothing here works around either.

use crate::output::print_json;
use clap::{Args as ClapArgs, Subcommand};
use rc_mobile::adb::{checklist, Adb, Checklist};
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum AndroidCmd {
    /// Show what is and is not ready: adb, connection, authorization, lock state.
    Checklist(Target),
    /// List trashed media, thumbnails, app media caches and databases on the phone.
    Survey(Target),
    /// Pull everything `survey` lists into a new directory, with a manifest.
    Pull(PullArgs),
}

#[derive(ClapArgs)]
pub struct Target {
    /// Which phone, when more than one is connected.
    #[arg(long)]
    serial: Option<String>,
    /// Confirm the phone is unlocked when its Android version does not report
    /// lock state. Never overrides a lock screen that is reported as showing.
    #[arg(long)]
    confirm_unlocked: bool,
}

#[derive(ClapArgs)]
pub struct PullArgs {
    #[command(flatten)]
    target: Target,
    /// A new or empty directory on this computer.
    #[arg(long)]
    out: PathBuf,
}

fn print_checklist(c: &Checklist) {
    let mark = |b: bool| if b { "[x]" } else { "[ ]" };
    println!("{} adb found", mark(c.adb_found));
    println!("{} phone connected", mark(c.device_connected));
    println!(
        "{} USB debugging authorized for this computer",
        mark(c.authorized)
    );
    println!(
        "{} phone unlocked{}",
        mark(c.unlocked == Some(true)),
        if c.unlocked.is_none() && c.authorized {
            " (not reported by this Android version)"
        } else {
            ""
        }
    );
    if let (Some(s), Some(m)) = (&c.serial, &c.model) {
        println!(
            "    {m} ({s}), API {}",
            c.api_level.map_or("?".into(), |l| l.to_string())
        );
    }
    for p in &c.problems {
        println!("  - {p}");
    }
}

fn ready_adb(t: &Target, json: bool) -> anyhow::Result<Option<(Adb, Checklist)>> {
    let adb = Adb::find();
    let c = checklist(adb.as_ref(), t.serial.as_deref());
    if !c.ready(t.confirm_unlocked) {
        if json {
            print_json(&c)?;
        } else {
            print_checklist(&c);
        }
        anyhow::bail!("the phone is not ready; see the checklist above");
    }
    let adb = adb
        .expect("ready implies adb")
        .with_serial(c.serial.as_deref().unwrap_or(""));
    Ok(Some((adb, c)))
}

pub fn android(cmd: AndroidCmd, json: bool) -> anyhow::Result<()> {
    match cmd {
        AndroidCmd::Checklist(t) => {
            let adb = Adb::find();
            let c = checklist(adb.as_ref(), t.serial.as_deref());
            if json {
                print_json(&c)
            } else {
                print_checklist(&c);
                Ok(())
            }
        }
        AndroidCmd::Survey(t) => {
            let Some((adb, _)) = ready_adb(&t, json)? else {
                return Ok(());
            };
            let files = rc_mobile::android::survey(&adb)?;
            let rows = rc_mobile::android::trashed_mediastore_rows(&adb).unwrap_or_default();
            if json {
                return print_json(
                    &serde_json::json!({"files": files, "mediastore_trashed": rows}),
                );
            }
            println!(
                "{} files ({} trashed); MediaStore reports {} trashed rows",
                files.len(),
                files
                    .iter()
                    .filter(|f| f.category == rc_mobile::android::Category::Trashed)
                    .count(),
                rows.len()
            );
            for f in &files {
                println!(
                    "{:<10} {:>10}  {}{}",
                    format!("{:?}", f.category),
                    f.size,
                    f.path,
                    f.original_name
                        .as_ref()
                        .map(|n| format!("  (was {n})"))
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
        AndroidCmd::Pull(a) => {
            let Some((adb, c)) = ready_adb(&a.target, json)? else {
                return Ok(());
            };
            let files = rc_mobile::android::survey(&adb)?;
            let pulled = rc_mobile::android::pull_all(&adb, &files, &a.out)?;
            if json {
                return print_json(&pulled);
            }
            let short = pulled.iter().filter(|p| !p.size_matches).count();
            println!(
                "pulled {} files from {} into {} ({} with a size different from the listing)",
                pulled.len(),
                c.model.unwrap_or_default(),
                a.out.display(),
                short
            );
            if let Err(why) = rc_mobile::android::adb_backup_available(c.api_level) {
                println!("adb backup: {why}");
            }
            Ok(())
        }
    }
}

#[derive(Subcommand)]
pub enum IosCmd {
    /// List iOS backups on this computer.
    Backups,
    /// List the "Recently Deleted" photos and videos in a backup.
    Trashed(IosBackupArgs),
    /// Copy the files of "Recently Deleted" assets out of a backup.
    Extract(IosExtractArgs),
    /// Start a new backup of a connected iPhone that trusts this computer
    /// (needs libimobiledevice's idevicebackup2).
    Backup(IosNewBackupArgs),
}

#[derive(ClapArgs)]
pub struct IosBackupArgs {
    /// The backup directory (the one holding Manifest.db).
    backup: PathBuf,
}

#[derive(ClapArgs)]
pub struct IosExtractArgs {
    backup: PathBuf,
    /// A new or empty directory.
    #[arg(long)]
    out: PathBuf,
}

#[derive(ClapArgs)]
pub struct IosNewBackupArgs {
    /// Where to write the backup.
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    udid: Option<String>,
}

pub fn ios(cmd: IosCmd, json: bool) -> anyhow::Result<()> {
    match cmd {
        IosCmd::Backups => {
            let roots = rc_mobile::host::Roots::from_env().ios_backup_roots();
            let found = rc_mobile::ios::find_backups(&roots);
            if json {
                return print_json(&found);
            }
            if found.is_empty() {
                println!("no iOS backups found in:");
                for r in roots {
                    println!("  {}", r.display());
                }
            }
            for b in found {
                println!(
                    "{}  {} ({} iOS {}){}",
                    b.dir.display(),
                    b.device_name.unwrap_or_default(),
                    b.product_type.unwrap_or_default(),
                    b.product_version.unwrap_or_default(),
                    if b.encrypted {
                        "  ENCRYPTED - not supported"
                    } else {
                        ""
                    }
                );
            }
            Ok(())
        }
        IosCmd::Trashed(a) => {
            let backup = rc_mobile::ios::Backup::open(&a.backup)?;
            let assets = backup.recently_deleted()?;
            if json {
                return print_json(&assets);
            }
            println!("{} assets in Recently Deleted", assets.len());
            for x in &assets {
                println!(
                    "{}/{}  (was {})  original {}  {} derivative(s)",
                    x.directory.as_deref().unwrap_or("?"),
                    x.filename.as_deref().unwrap_or("?"),
                    x.original_filename.as_deref().unwrap_or("?"),
                    match &x.original {
                        Some(f) if f.present => "in backup",
                        _ => "NOT in backup",
                    },
                    x.derivatives.len()
                );
            }
            Ok(())
        }
        IosCmd::Extract(a) => {
            let backup = rc_mobile::ios::Backup::open(&a.backup)?;
            let assets = backup.recently_deleted()?;
            let done = rc_mobile::ios::extract(&assets, &a.out)?;
            if json {
                return print_json(&done);
            }
            println!("copied {} files into {}", done.len(), a.out.display());
            Ok(())
        }
        IosCmd::Backup(a) => {
            rc_mobile::ios::start_backup(&a.out, a.udid.as_deref())?;
            println!("backup written to {}", a.out.display());
            Ok(())
        }
    }
}

pub fn host_backups(json: bool) -> anyhow::Result<()> {
    let found = rc_mobile::host::discover(&rc_mobile::host::Roots::from_env());
    if json {
        return print_json(&found);
    }
    if found.is_empty() {
        println!("no phone backups or sync caches found under this profile");
    }
    for f in found {
        println!(
            "{:<14} {:>8} files {:>12} bytes {:>6} placeholders  {}",
            format!("{:?}", f.kind),
            f.files,
            f.bytes,
            f.placeholders,
            f.path.display()
        );
    }
    Ok(())
}

#[cfg(feature = "bridge")]
#[derive(ClapArgs)]
pub struct BridgeArgs {
    /// Port on 127.0.0.1, forwarded from the phone with adb reverse.
    #[arg(long, default_value_t = 38300)]
    port: u16,
    /// A new or empty directory.
    #[arg(long)]
    out: PathBuf,
    /// Which phone, when more than one is connected.
    #[arg(long)]
    serial: Option<String>,
}

#[cfg(feature = "bridge")]
pub fn bridge(a: BridgeArgs, json: bool) -> anyhow::Result<()> {
    let rx = rc_mobile::bridge::Receiver::bind(a.port, &a.out)?;
    let adb = Adb::find().ok_or_else(|| anyhow::anyhow!("adb was not found"))?;
    let adb = match &a.serial {
        Some(s) => adb.with_serial(s),
        None => adb,
    };
    rc_mobile::bridge::adb_reverse(&adb, rx.port())?;
    eprintln!(
        "Listening on 127.0.0.1:{} (reachable from the phone over USB only).\n\
         In the companion app, tap 'Send to computer' and enter code {}",
        rx.port(),
        rx.code()
    );
    let files = rx.serve(std::time::Duration::from_secs(120))?;
    if json {
        return print_json(&files);
    }
    println!("received {} files into {}", files.len(), a.out.display());
    Ok(())
}
