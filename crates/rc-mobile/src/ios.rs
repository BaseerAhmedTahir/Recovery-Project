//! iOS backups: find them, parse `Manifest.db`, and list "Recently Deleted"
//! photos and videos from `Photos.sqlite` (SPEC.md 6.2).
//!
//! A backup directory holds every file under the SHA-1 of `domain-relativePath`
//! (`ab/abcdef...`), and `Manifest.db`'s `Files` table maps them back. Photos
//! live in `CameraRollDomain`: originals under `Media/DCIM/`, the library
//! database at `Media/PhotoData/Photos.sqlite`, and reduced renders under
//! `Media/PhotoData/Thumbnails/` and `Mutations/`.
//!
//! An asset in "Recently Deleted" has `ZTRASHEDSTATE = 1` in `ZASSET`
//! (`ZGENERICASSET` before iOS 14), with the time it was trashed in
//! `ZTRASHEDDATE`; its original file name is in `ZADDITIONALASSETATTRIBUTES`.
//!
//! Both databases are read with `rc-sqlite-carve`'s page reader, not SQLite, so
//! nothing is written into the backup directory.
//!
//! Not implemented: **encrypted backups** (detected and refused with an
//! explanation - decryption needs the backup password and was not built because
//! nothing on this machine can make an encrypted backup to test it against),
//! and pre-iOS 10 backups (`Manifest.mbdb`). Starting a new backup needs
//! `idevicebackup2` from libimobiledevice and a phone that trusts this computer.

use crate::{Error, Result};
use rc_sqlite_carve::format::Value;
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Seconds between the Unix epoch and Core Data's (2001-01-01).
const CORE_DATA_EPOCH: f64 = 978_307_200.0;

#[derive(Clone, Debug, Serialize)]
pub struct BackupInfo {
    pub dir: PathBuf,
    pub device_name: Option<String>,
    pub product_type: Option<String>,
    pub product_version: Option<String>,
    pub encrypted: bool,
    pub last_backup: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BackupFile {
    pub file_id: String,
    pub domain: String,
    pub relative_path: String,
    /// Where the bytes are in the backup directory.
    pub stored_at: PathBuf,
    pub present: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct TrashedAsset {
    pub pk: i64,
    pub directory: Option<String>,
    pub filename: Option<String>,
    pub original_filename: Option<String>,
    /// 0 photo, 1 video.
    pub kind: Option<i64>,
    pub uniform_type: Option<String>,
    pub trashed_unix: Option<i64>,
    pub created_unix: Option<i64>,
    pub original: Option<BackupFile>,
    /// Thumbnails and edited renders of the same asset.
    pub derivatives: Vec<BackupFile>,
}

pub fn file_id(domain: &str, relative_path: &str) -> String {
    hex::encode(Sha1::digest(format!("{domain}-{relative_path}").as_bytes()))
}

fn plist_dict(path: &Path) -> Option<plist::Dictionary> {
    plist::Value::from_file(path).ok()?.into_dictionary()
}

fn dict_str(d: &plist::Dictionary, k: &str) -> Option<String> {
    match d.get(k)? {
        plist::Value::String(s) => Some(s.clone()),
        plist::Value::Date(t) => Some(t.to_xml_format()),
        _ => None,
    }
}

pub fn read_info(dir: &Path) -> Result<BackupInfo> {
    let manifest = plist_dict(&dir.join("Manifest.plist"))
        .ok_or_else(|| Error::Backup(format!("{}: no readable Manifest.plist", dir.display())))?;
    let info = plist_dict(&dir.join("Info.plist")).unwrap_or_default();
    let lockdown = manifest
        .get("Lockdown")
        .and_then(|v| v.as_dictionary())
        .cloned()
        .unwrap_or_default();
    Ok(BackupInfo {
        dir: dir.to_path_buf(),
        device_name: dict_str(&info, "Device Name").or_else(|| dict_str(&lockdown, "DeviceName")),
        product_type: dict_str(&info, "Product Type")
            .or_else(|| dict_str(&lockdown, "ProductType")),
        product_version: dict_str(&info, "Product Version")
            .or_else(|| dict_str(&lockdown, "ProductVersion")),
        encrypted: matches!(
            manifest.get("IsEncrypted"),
            Some(plist::Value::Boolean(true))
        ),
        last_backup: dict_str(&info, "Last Backup Date"),
    })
}

/// Backup directories under each root (a `MobileSync/Backup` directory).
pub fn find_backups(roots: &[PathBuf]) -> Vec<BackupInfo> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.join("Manifest.plist").is_file())
            .collect();
        dirs.sort();
        out.extend(dirs.iter().filter_map(|d| read_info(d).ok()));
    }
    out
}

fn text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Text(t) => Some(t.clone()),
        _ => None,
    }
}

fn int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Integer(i) => Some(*i),
        Value::Real(f) => Some(*f as i64),
        _ => None,
    }
}

fn core_data_time(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Real(f) => Some((f + CORE_DATA_EPOCH) as i64),
        Value::Integer(i) => Some(*i + CORE_DATA_EPOCH as i64),
        _ => None,
    }
}

pub struct Backup {
    pub info: BackupInfo,
    /// relativePath -> file, for CameraRollDomain; every domain by id.
    camera_roll: HashMap<String, BackupFile>,
}

impl Backup {
    pub fn open(dir: &Path) -> Result<Backup> {
        let info = read_info(dir)?;
        if info.encrypted {
            return Err(Error::Backup(
                "this backup is encrypted. Decrypting it needs the backup password, and \
                 encrypted-backup support is not implemented; make an unencrypted backup \
                 (Finder/iTunes: untick 'Encrypt local backup') to use this"
                    .into(),
            ));
        }
        let manifest_db = dir.join("Manifest.db");
        if !manifest_db.is_file() {
            return Err(Error::Backup(if dir.join("Manifest.mbdb").is_file() {
                "this is a pre-iOS 10 backup (Manifest.mbdb), which is not supported".into()
            } else {
                "no Manifest.db in this backup".into()
            }));
        }
        let bytes = std::fs::read(&manifest_db)?;
        let (cols, rows) = rc_sqlite_carve::read_table(&bytes, None, "Files")
            .map_err(|e| Error::Backup(format!("Manifest.db: {e}")))?;
        let idx = |name: &str| cols.iter().position(|c| c.name == name);
        let (Some(fid), Some(dom), Some(rel)) = (idx("fileID"), idx("domain"), idx("relativePath"))
        else {
            return Err(Error::Backup(
                "Manifest.db has an unfamiliar Files table".into(),
            ));
        };
        let flags = idx("flags");
        let mut camera_roll = HashMap::new();
        for r in rows {
            let (Some(id), Some(domain), Some(path)) =
                (text(r.get(fid)), text(r.get(dom)), text(r.get(rel)))
            else {
                continue;
            };
            if domain != "CameraRollDomain" || flags.and_then(|f| int(r.get(f))) == Some(2) {
                continue;
            }
            let stored_at = dir.join(id.get(..2).unwrap_or("")).join(&id);
            camera_roll.insert(
                path.clone(),
                BackupFile {
                    present: stored_at.is_file(),
                    file_id: id,
                    domain,
                    relative_path: path,
                    stored_at,
                },
            );
        }
        Ok(Backup { info, camera_roll })
    }

    pub fn camera_roll_files(&self) -> usize {
        self.camera_roll.len()
    }

    /// Assets in "Recently Deleted".
    pub fn recently_deleted(&self) -> Result<Vec<TrashedAsset>> {
        let photos = self
            .camera_roll
            .get("Media/PhotoData/Photos.sqlite")
            .filter(|f| f.present)
            .ok_or_else(|| Error::Backup("the backup has no Photos.sqlite".into()))?;
        let db = std::fs::read(&photos.stored_at)?;
        let wal = self
            .camera_roll
            .get("Media/PhotoData/Photos.sqlite-wal")
            .and_then(|f| std::fs::read(&f.stored_at).ok());
        let read = |t: &str| rc_sqlite_carve::read_table(&db, wal.as_deref(), t);
        let (cols, rows) = match read("ZASSET") {
            Ok(x) => x,
            Err(_) => read("ZGENERICASSET")
                .map_err(|e| Error::Backup(format!("Photos.sqlite: no asset table ({e})")))?,
        };
        let idx = |name: &str| cols.iter().position(|c| c.name == name);
        let trashed = idx("ZTRASHEDSTATE").ok_or_else(|| {
            Error::Backup(
                "Photos.sqlite has no ZTRASHEDSTATE column (iOS version not supported)".into(),
            )
        })?;
        let pk = idx("Z_PK").ok_or_else(|| Error::Backup("no Z_PK column".into()))?;

        let mut originals: HashMap<i64, String> = HashMap::new();
        if let Ok((acols, arows)) = read("ZADDITIONALASSETATTRIBUTES") {
            let a = acols.iter().position(|c| c.name == "ZASSET");
            let n = acols.iter().position(|c| c.name == "ZORIGINALFILENAME");
            if let (Some(a), Some(n)) = (a, n) {
                for r in arows {
                    if let (Some(asset), Some(name)) = (int(r.get(a)), text(r.get(n))) {
                        originals.insert(asset, name);
                    }
                }
            }
        }

        let mut out = Vec::new();
        for r in rows {
            if int(r.get(trashed)) != Some(1) {
                continue;
            }
            let Some(key) = int(r.get(pk)) else { continue };
            let get = |name: &str| idx(name).and_then(|i| r.get(i));
            let directory = text(get("ZDIRECTORY"));
            let filename = text(get("ZFILENAME"));
            let (original, derivatives) = match (&directory, &filename) {
                (Some(d), Some(f)) => self.files_for(d, f),
                _ => (None, Vec::new()),
            };
            out.push(TrashedAsset {
                pk: key,
                original_filename: originals.get(&key).cloned(),
                kind: int(get("ZKIND")),
                uniform_type: text(get("ZUNIFORMTYPEIDENTIFIER")),
                trashed_unix: core_data_time(get("ZTRASHEDDATE")),
                created_unix: core_data_time(get("ZDATECREATED")),
                directory,
                filename,
                original,
                derivatives,
            });
        }
        out.sort_by_key(|a| a.pk);
        Ok(out)
    }

    fn files_for(&self, directory: &str, filename: &str) -> (Option<BackupFile>, Vec<BackupFile>) {
        let original = self
            .camera_roll
            .get(&format!("Media/{directory}/{filename}"))
            .cloned();
        let stem = filename.rsplit_once('.').map_or(filename, |s| s.0);
        let mut derivatives: Vec<BackupFile> = self
            .camera_roll
            .values()
            .filter(|f| {
                let p = &f.relative_path;
                p.starts_with("Media/PhotoData/")
                    && !p.starts_with("Media/PhotoData/Photos.sqlite")
                    && (p.contains(&format!("/{directory}/{filename}/"))
                        || p.contains(&format!("/{directory}/{stem}/")))
            })
            .cloned()
            .collect();
        derivatives.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        (original, derivatives)
    }
}

/// Copy the files of trashed assets out of the backup into `out`, which must be
/// empty or not exist. Returns (source, destination) pairs.
pub fn extract(assets: &[TrashedAsset], out: &Path) -> Result<Vec<(BackupFile, PathBuf)>> {
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        return Err(Error::Destination(format!(
            "{} is not empty; choose a new directory",
            out.display()
        )));
    }
    std::fs::create_dir_all(out)?;
    let mut done = Vec::new();
    for a in assets {
        for f in a.original.iter().chain(&a.derivatives) {
            if !f.present {
                continue;
            }
            let mut dest = out.to_path_buf();
            for part in f.relative_path.split('/') {
                if part.is_empty() || part == "." || part == ".." {
                    continue;
                }
                dest.push(part);
            }
            if let Some(p) = dest.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(&f.stored_at, &dest)?;
            done.push((f.clone(), dest));
        }
    }
    Ok(done)
}

/// Start a full backup of a trusted, unlocked iPhone with libimobiledevice's
/// `idevicebackup2`. The phone must already trust this computer; iOS may ask
/// for the passcode on the phone. Not verified on a real device.
pub fn start_backup(dest: &Path, udid: Option<&str>) -> Result<()> {
    let exe = if cfg!(windows) {
        "idevicebackup2.exe"
    } else {
        "idevicebackup2"
    };
    let mut cmd = Command::new(exe);
    if let Some(u) = udid {
        cmd.arg("-u").arg(u);
    }
    std::fs::create_dir_all(dest)?;
    let status = cmd
        .arg("backup")
        .arg("--full")
        .arg(dest)
        .status()
        .map_err(|e| Error::Backup(format!("could not run {exe} (libimobiledevice): {e}")))?;
    if !status.success() {
        return Err(Error::Backup(format!("{exe} failed with {status}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_ids_are_sha1_of_domain_and_path() {
        // The well-known id of Photos.sqlite in every iOS backup.
        assert_eq!(
            file_id("CameraRollDomain", "Media/PhotoData/Photos.sqlite"),
            "12b144c0bd44f2b3dffd9186d3f9c05b917cee25"
        );
    }
}
