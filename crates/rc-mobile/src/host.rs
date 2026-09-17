//! Phone backups and sync caches already on this computer (SPEC.md 6.3) -
//! often the richest source, and no phone is needed.
//!
//! Discovery only looks in the documented locations under the user's own
//! profile; it never searches other users' profiles. What each place holds:
//!
//! | Kind | Where | What is done with it |
//! |---|---|---|
//! | iOS backup | `MobileSync/Backup` (macOS; Windows iTunes and Microsoft Store Apple Devices variants) | parsed by [`crate::ios`] |
//! | Samsung Smart Switch | `Documents/Samsung/SmartSwitch/backup` | listed; files identified by signature |
//! | Google Drive (DriveFS) | `DriveFS/<account>/content_cache` | cached chunks identified by signature |
//! | OneDrive | the OneDrive folders under the profile | listed; cloud-only placeholders hold no data |
//! | iCloud Drive | `Mobile Documents` (macOS), `iCloudDrive` (Windows) | listed; `.icloud` placeholders hold no data |
//!
//! Content identification by signature is real (it uses the carving signature
//! database). Mapping DriveFS cache chunks back to their original names through
//! its metadata database is **not implemented**: the schema is undocumented and
//! changes between versions, and there is no sample here to check it against.

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    IosBackup,
    SmartSwitch,
    DriveFsCache,
    OneDrive,
    ICloudDrive,
}

#[derive(Clone, Debug, Serialize)]
pub struct Found {
    pub kind: Kind,
    pub path: PathBuf,
    pub files: u64,
    pub bytes: u64,
    /// Files that are placeholders for content not stored locally.
    pub placeholders: u64,
}

/// The directories discovery looks in. Built from the environment by
/// [`Roots::from_env`]; tests build one around a fake profile.
#[derive(Clone, Debug, Default)]
pub struct Roots {
    pub home: Option<PathBuf>,
    /// `%LOCALAPPDATA%` on Windows.
    pub local_app_data: Option<PathBuf>,
    /// `%APPDATA%` on Windows.
    pub app_data: Option<PathBuf>,
}

impl Roots {
    pub fn from_env() -> Roots {
        let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
        Roots {
            home: var("USERPROFILE").or_else(|| var("HOME")),
            local_app_data: var("LOCALAPPDATA"),
            app_data: var("APPDATA"),
        }
    }

    /// `MobileSync/Backup` directories, for [`crate::ios::find_backups`].
    pub fn ios_backup_roots(&self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(h) = &self.home {
            v.push(h.join("Library/Application Support/MobileSync/Backup"));
            v.push(h.join("Apple").join("MobileSync").join("Backup"));
        }
        if let Some(a) = &self.app_data {
            v.push(a.join("Apple Computer").join("MobileSync").join("Backup"));
        }
        v
    }

    fn candidates(&self) -> Vec<(Kind, PathBuf)> {
        let mut v: Vec<(Kind, PathBuf)> = self
            .ios_backup_roots()
            .into_iter()
            .map(|p| (Kind::IosBackup, p))
            .collect();
        if let Some(h) = &self.home {
            v.push((
                Kind::SmartSwitch,
                h.join("Documents")
                    .join("Samsung")
                    .join("SmartSwitch")
                    .join("backup"),
            ));
            v.push((
                Kind::DriveFsCache,
                h.join("Library/Application Support/Google/DriveFS"),
            ));
            v.push((Kind::OneDrive, h.join("OneDrive")));
            v.push((Kind::ICloudDrive, h.join("Library/Mobile Documents")));
            v.push((Kind::ICloudDrive, h.join("iCloudDrive")));
        }
        if let Some(l) = &self.local_app_data {
            v.push((Kind::DriveFsCache, l.join("Google").join("DriveFS")));
        }
        v
    }
}

/// Stop counting a tree after this many entries; the summary says "at least".
const WALK_LIMIT: u64 = 2_000_000;

fn walk(dir: &Path, only: &dyn Fn(&Path) -> bool, f: &mut Found) {
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0u64;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            seen += 1;
            if seen > WALK_LIMIT {
                return;
            }
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() && only(&p) {
                f.files += 1;
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                f.bytes += len;
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".icloud") || is_offline_placeholder(&e) {
                    f.placeholders += 1;
                }
            }
        }
    }
}

#[cfg(windows)]
fn is_offline_placeholder(e: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    // FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | RECALL_ON_OPEN
    const MASK: u32 = 0x1000 | 0x0040_0000 | 0x0004_0000;
    e.metadata()
        .map(|m| m.file_attributes() & MASK != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_offline_placeholder(_: &std::fs::DirEntry) -> bool {
    false
}

pub fn discover(roots: &Roots) -> Vec<Found> {
    let mut out = Vec::new();
    for (kind, path) in roots.candidates() {
        if !path.is_dir() {
            continue;
        }
        match kind {
            Kind::DriveFsCache => {
                // One content_cache per signed-in account.
                let Ok(rd) = std::fs::read_dir(&path) else {
                    continue;
                };
                for acct in rd.filter_map(|e| e.ok()).map(|e| e.path()) {
                    let cache = acct.join("content_cache");
                    if cache.is_dir() {
                        let mut f = Found {
                            kind,
                            path: cache.clone(),
                            files: 0,
                            bytes: 0,
                            placeholders: 0,
                        };
                        walk(&cache, &|_| true, &mut f);
                        out.push(f);
                    }
                }
            }
            _ => {
                let mut f = Found {
                    kind,
                    path: path.clone(),
                    files: 0,
                    bytes: 0,
                    placeholders: 0,
                };
                walk(&path, &|_| true, &mut f);
                out.push(f);
            }
        }
    }
    out
}

/// What a cached blob is, by its leading bytes, using the carving signature
/// database. `None` when nothing matches.
pub fn identify(head: &[u8], sigs: &rc_carve::SignatureDb) -> Option<String> {
    sigs.signatures
        .iter()
        .find(|s| s.matches(head))
        .map(|s| s.ext.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_each_kind_in_a_fake_profile() {
        let tmp = std::env::temp_dir().join(format!("rc-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let home = tmp.join("home");
        let local = tmp.join("local");
        let appdata = tmp.join("roaming");
        let put = |p: PathBuf, n: usize| {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![7u8; n]).unwrap();
        };
        put(
            appdata.join("Apple Computer/MobileSync/Backup/abc/Manifest.plist"),
            10,
        );
        put(
            home.join("Documents/Samsung/SmartSwitch/backup/SM-G973F/Message/sms.bk"),
            20,
        );
        put(
            local.join("Google/DriveFS/1234/content_cache/d1/d2/5678"),
            30,
        );
        put(home.join("iCloudDrive/Docs/.report.pdf.icloud"), 5);
        let roots = Roots {
            home: Some(home),
            local_app_data: Some(local),
            app_data: Some(appdata),
        };
        let found = discover(&roots);
        let kinds: Vec<Kind> = found.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            vec![
                Kind::IosBackup,
                Kind::SmartSwitch,
                Kind::ICloudDrive,
                Kind::DriveFsCache
            ]
        );
        assert_eq!(found[0].bytes, 10);
        assert_eq!(found[2].placeholders, 1);
        assert_eq!(found[3].bytes, 30);
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
