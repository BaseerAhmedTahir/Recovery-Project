//! Logical extraction from an Android phone over adb, in yield order
//! (SPEC.md 6.1). Block-level carving of the phone is impossible without
//! root and pointless with it (file-based encryption; SPEC.md 1.2), so this
//! pulls files that still exist on shared storage into a working directory on
//! the computer, where the ordinary pipeline takes over.
//!
//! 1. Trashed media (Android 11+): MediaStore rows with `is_trashed=1` and the
//!    `.trashed-<expiry>-<name>` files behind them. Found by listing shared
//!    storage, which works whatever MediaStore's query filtering does; the
//!    MediaStore query is also tried and its rows reported.
//! 2. Thumbnails and app media caches.
//! 3. SQLite databases on shared storage, with their `-wal`/`-journal`, for
//!    `rc-sqlite-carve`. App-private databases (`/data/data`) are not readable
//!    without root and are not attempted.
//! 4. `adb backup`, only below API 31, where it still does something; it needs
//!    the user to confirm on the phone.
//!
//! Every pulled file is hashed and recorded in `manifest.json` with where it
//! came from.

use crate::adb::{shell_quote, Adb};
use crate::{Error, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const SHARED: &str = "/storage/emulated/0";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Trashed,
    Thumbnail,
    AppMedia,
    Database,
}

/// A file on the phone.
#[derive(Clone, Debug, Serialize)]
pub struct RemoteFile {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
    pub category: Category,
    /// For `.trashed-<expiry>-<name>`: the name it had and when it expires.
    pub original_name: Option<String>,
    pub expires_unix: Option<i64>,
}

/// A MediaStore row reported as trashed.
#[derive(Clone, Debug, Serialize)]
pub struct TrashedRow {
    pub id: Option<u64>,
    pub data: Option<String>,
    pub display_name: Option<String>,
    pub mime_type: Option<String>,
    pub size: Option<u64>,
    pub date_expires: Option<i64>,
}

/// `.trashed-1726000000-IMG_1.jpg` -> (1726000000, "IMG_1.jpg").
pub fn parse_trashed_name(name: &str) -> Option<(i64, String)> {
    let rest = name.strip_prefix(".trashed-")?;
    let (expiry, original) = rest.split_once('-')?;
    let expiry: i64 = expiry.parse().ok()?;
    (!original.is_empty()).then(|| (expiry, original.to_string()))
}

/// Parse `find ... -exec stat -c '%s|%Y|%n' {} +` output.
pub fn parse_stat_lines(text: &str, category: Category) -> Vec<RemoteFile> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '|');
            let size = it.next()?.trim().parse().ok()?;
            let mtime = it.next()?.trim().parse().ok()?;
            let path = it.next()?.to_string();
            let name = path.rsplit('/').next().unwrap_or(&path);
            let parsed = parse_trashed_name(name);
            Some(RemoteFile {
                size,
                mtime,
                category,
                original_name: parsed.as_ref().map(|p| p.1.clone()),
                expires_unix: parsed.map(|p| p.0),
                path,
            })
        })
        .collect()
}

/// Parse `content query` output: `Row: 0 _id=31, _data=/storage/..., ...`.
///
/// Values may contain ", "; a field starts only where ", key=" names one of the
/// requested columns.
pub fn parse_content_rows(text: &str, columns: &[&str]) -> Vec<Vec<(String, String)>> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("Row: ") else {
            continue;
        };
        let Some((_, body)) = rest.split_once(' ') else {
            continue;
        };
        let mut starts: Vec<(usize, &str)> = Vec::new();
        for col in columns {
            let first = format!("{col}=");
            if body.starts_with(&first) {
                starts.push((0, col));
            }
            let mid = format!(", {col}=");
            for (i, _) in body.match_indices(&mid) {
                starts.push((i + 2, col));
            }
        }
        starts.sort();
        let mut row = Vec::new();
        for (k, &(at, col)) in starts.iter().enumerate() {
            let vstart = at + col.len() + 1;
            let vend = starts.get(k + 1).map_or(body.len(), |n| n.0 - 2);
            if vstart <= vend {
                row.push((col.to_string(), body[vstart..vend].to_string()));
            }
        }
        rows.push(row);
    }
    rows
}

const TRASH_COLUMNS: &[&str] = &[
    "_id",
    "_data",
    "_display_name",
    "mime_type",
    "_size",
    "date_expires",
    "is_trashed",
];

pub fn trashed_mediastore_rows(adb: &Adb) -> Result<Vec<TrashedRow>> {
    let cmd = format!(
        "content query --uri content://media/external/file --projection {} --where {}",
        TRASH_COLUMNS.join(":"),
        shell_quote("is_trashed=1")
    );
    let out = adb.shell(&cmd)?;
    Ok(parse_content_rows(&out, TRASH_COLUMNS)
        .into_iter()
        .map(|r| {
            let get = |k: &str| {
                r.iter()
                    .find(|(c, _)| c == k)
                    .map(|(_, v)| v.clone())
                    .filter(|v| v != "NULL")
            };
            TrashedRow {
                id: get("_id").and_then(|v| v.parse().ok()),
                data: get("_data"),
                display_name: get("_display_name"),
                mime_type: get("mime_type"),
                size: get("_size").and_then(|v| v.parse().ok()),
                date_expires: get("date_expires").and_then(|v| v.parse().ok()),
            }
        })
        .collect())
}

fn find(adb: &Adb, root: &str, expr: &str, category: Category) -> Result<Vec<RemoteFile>> {
    let cmd = format!(
        "find {} {expr} -type f -exec stat -c '%s|%Y|%n' {{}} + 2>/dev/null",
        shell_quote(root)
    );
    Ok(parse_stat_lines(&adb.shell(&cmd)?, category))
}

/// Everything worth pulling, by category.
pub fn survey(adb: &Adb) -> Result<Vec<RemoteFile>> {
    let mut all = find(adb, SHARED, "-name '.trashed-*'", Category::Trashed)?;
    all.extend(find(
        adb,
        &format!("{SHARED}/DCIM/.thumbnails"),
        "",
        Category::Thumbnail,
    )?);
    for dir in [
        "Android/media/com.whatsapp/WhatsApp/Media",
        "Android/media/org.telegram.messenger",
        "Telegram",
        "Pictures/.thumbnails",
    ] {
        all.extend(find(
            adb,
            &format!("{SHARED}/{dir}"),
            "",
            Category::AppMedia,
        )?);
    }
    all.extend(find(
        adb,
        SHARED,
        "\\( -name '*.db' -o -name '*.sqlite' -o -name '*.db-wal' -o -name '*.db-journal' \\)",
        Category::Database,
    )?);
    all.sort_by(|a, b| a.path.cmp(&b.path));
    all.dedup_by(|a, b| a.path == b.path);
    Ok(all)
}

#[derive(Clone, Debug, Serialize)]
pub struct Pulled {
    pub remote: RemoteFile,
    pub local: PathBuf,
    pub sha256: String,
    pub size_matches: bool,
}

/// Pull `files` into `out/<category>/<path on the phone>`, hash each, and write
/// `out/manifest.json`. `out` must be empty or not exist.
pub fn pull_all(adb: &Adb, files: &[RemoteFile], out: &Path) -> Result<Vec<Pulled>> {
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        return Err(Error::Destination(format!(
            "{} is not empty; choose a new directory",
            out.display()
        )));
    }
    std::fs::create_dir_all(out)?;
    let mut pulled = Vec::new();
    for f in files {
        let cat = match f.category {
            Category::Trashed => "trashed",
            Category::Thumbnail => "thumbnails",
            Category::AppMedia => "app-media",
            Category::Database => "databases",
        };
        let mut local = out.join(cat);
        for part in f.path.trim_start_matches('/').split('/') {
            // Nothing from the phone may climb out of the output directory.
            if part.is_empty() || part == "." || part == ".." {
                continue;
            }
            local.push(sanitize(part));
        }
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)?;
        }
        adb.pull(&f.path, &local)?;
        let bytes = std::fs::read(&local)?;
        pulled.push(Pulled {
            size_matches: bytes.len() as u64 == f.size,
            sha256: hex::encode(Sha256::digest(&bytes)),
            remote: f.clone(),
            local,
        });
    }
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&pulled).map_err(|e| Error::Other(e.to_string()))?,
    )?;
    Ok(pulled)
}

/// Characters Windows refuses in a file name.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

/// `adb backup` stopped doing anything useful in Android 12 (API 31).
pub fn adb_backup_available(api_level: Option<u32>) -> std::result::Result<(), String> {
    match api_level {
        Some(l) if l < 31 => Ok(()),
        Some(l) => Err(format!(
            "adb backup is a no-op from Android 12 (API 31); this phone is API {l}"
        )),
        None => Err("the API level is unknown, so adb backup is not attempted".into()),
    }
}

/// Run `adb backup -all -shared` into `dest`. The phone asks its holder to
/// confirm; nothing happens until they do.
pub fn adb_backup(adb: &Adb, api_level: Option<u32>, dest: &Path) -> Result<()> {
    adb_backup_available(api_level).map_err(Error::Adb)?;
    if dest.exists() {
        return Err(Error::Destination(format!("{} exists", dest.display())));
    }
    let d = dest.to_string_lossy().to_string();
    adb.run(&["backup", "-f", &d, "-all", "-shared", "-noapk"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trashed_names() {
        assert_eq!(
            parse_trashed_name(".trashed-1726000000-IMG_20240901_120000.jpg"),
            Some((1726000000, "IMG_20240901_120000.jpg".into()))
        );
        assert_eq!(
            parse_trashed_name(".trashed-1726000000-a-b.jpg").unwrap().1,
            "a-b.jpg"
        );
        assert!(parse_trashed_name("IMG.jpg").is_none());
        assert!(parse_trashed_name(".trashed-x-IMG.jpg").is_none());
    }

    #[test]
    fn content_query_rows_with_commas_in_values() {
        let text = "Row: 0 _id=31, _data=/storage/emulated/0/DCIM/.trashed-1-a, b.jpg, \
                    _display_name=.trashed-1-a, b.jpg, mime_type=image/jpeg, _size=1234, \
                    date_expires=1726000000, is_trashed=1\nNo result found.\n";
        let rows = parse_content_rows(text, TRASH_COLUMNS);
        assert_eq!(rows.len(), 1);
        let get = |k: &str| rows[0].iter().find(|(c, _)| c == k).unwrap().1.clone();
        assert_eq!(get("_data"), "/storage/emulated/0/DCIM/.trashed-1-a, b.jpg");
        assert_eq!(get("_size"), "1234");
        assert_eq!(get("is_trashed"), "1");
    }

    #[test]
    fn stat_lines() {
        let f = parse_stat_lines(
            "2048|1725000000|/storage/emulated/0/DCIM/Camera/.trashed-1726000000-IMG_1.jpg\nbad\n",
            Category::Trashed,
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].original_name.as_deref(), Some("IMG_1.jpg"));
        assert_eq!(f[0].expires_unix, Some(1726000000));
    }

    #[test]
    fn backup_gated_on_api_level() {
        assert!(adb_backup_available(Some(30)).is_ok());
        assert!(adb_backup_available(Some(31)).is_err());
        assert!(adb_backup_available(None).is_err());
    }
}
