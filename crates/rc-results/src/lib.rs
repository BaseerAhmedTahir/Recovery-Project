//! The GUI's model (SPEC.md 2, 8): a disk-backed results table that scans
//! fill and the grid reads a window at a time, plus the operations the GUI
//! runs on a row - preview, hex, restore. The Tauri app is a thin shell over
//! this crate, so everything here is testable without a window.
//!
//! # Why the grid stays responsive at 10 million rows
//!
//! Nothing ever holds all rows. The table lives in SQLite on disk. A *view* is
//! either "every row in id order", which needs no memory at all, or, after a
//! filter or sort, the matching ids as `u32`s (40 MB for 10 million). The grid
//! asks for the rows it can see - a few dozen - by position in the view, which
//! is one indexed lookup per row. Building a filtered view is a table scan and
//! takes seconds at 10 million rows; it runs off the UI thread and the grid
//! says it is filtering. `tests/ten_million.rs` measures all of it.

use rc_fs::{DataLocation, Entry, Geometry};
use rc_score::{score_entry, Context, Occupancy, Rules};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Device(#[from] rc_device::DeviceError),
    #[error(transparent)]
    Fs(#[from] rc_fs::FsError),
    #[error(transparent)]
    Partition(#[from] rc_partition::PartitionError),
    #[error(transparent)]
    Score(#[from] rc_score::ScoreError),
    #[error(transparent)]
    Restore(#[from] rc_restore::Error),
    #[error(transparent)]
    Preview(#[from] rc_preview::PreviewError),
    #[error(transparent)]
    Session(#[from] rc_session::SessionError),
    #[error(transparent)]
    Carve(#[from] rc_carve::CarveError),
    #[error(transparent)]
    Index(#[from] rc_index::IndexError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// One row of the grid.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Row {
    pub id: i64,
    /// `deleted` (from a filesystem), `carved`, or `synthetic`.
    pub kind: String,
    pub source: String,
    pub name: String,
    pub path: String,
    pub ext: String,
    pub size: u64,
    /// GREEN / YELLOW / RED, or empty when not rated (carved candidates are
    /// rated by validation status instead).
    pub band: String,
    pub score: i64,
    /// Carved: valid / partial / rejected. Deleted: the path confidence.
    pub status: String,
    /// Device byte offset of the content's start, when known.
    pub offset: Option<u64>,
    pub reasons: String,
    pub modified: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Filter {
    /// Substring of the path, case-insensitive.
    pub text: String,
    /// Only rows whose original path lies inside this folder (relative to the
    /// volume's root, `/`-separated, case-insensitive). How "scan a folder"
    /// works: a deleted file is no longer in any folder on disk, so the whole
    /// volume is read and the rows are narrowed to where the files used to be.
    /// Carved rows have no original path and never match.
    #[serde(default)]
    pub folder: String,
    pub bands: Vec<String>,
    pub kinds: Vec<String>,
    pub exts: Vec<String>,
    pub min_size: Option<u64>,
    /// Column to sort by: name, path, ext, size, band, score, modified.
    pub sort: Option<String>,
    pub descending: bool,
}

impl Filter {
    fn is_identity(&self) -> bool {
        self.text.is_empty()
            && self.folder.is_empty()
            && self.bands.is_empty()
            && self.kinds.is_empty()
            && self.exts.is_empty()
            && self.min_size.is_none()
            && self.sort.is_none()
    }
}

enum View {
    /// Every row, by id. Ids are dense from 1, so position `i` is id `i+1`.
    All(u64),
    Ids(Vec<u32>),
}

pub struct Store {
    conn: Connection,
    view: View,
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS sources (id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL);
CREATE TABLE IF NOT EXISTS rows (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    source_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    ext TEXT NOT NULL,
    size INTEGER NOT NULL,
    band TEXT NOT NULL,
    score INTEGER NOT NULL,
    status TEXT NOT NULL,
    offset INTEGER,
    length INTEGER,
    reasons TEXT NOT NULL,
    modified INTEGER,
    detail TEXT
);
";

fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| e.len() <= 8)
        .unwrap_or_default()
}

/// What restoring or previewing a row needs beyond the grid columns. Boxed:
/// a deleted entry carries its whole run list, a carved one two strings.
#[derive(Serialize, Deserialize)]
enum Detail {
    Deleted {
        entry: Box<Entry>,
        cluster_bytes: u64,
        heap_offset: u64,
        first_cluster: u64,
        cluster_count: u64,
    },
    Carved {
        signature_id: String,
        validator: Option<String>,
    },
}

impl Store {
    /// Open or create the results database at `path`.
    pub fn open(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        let n: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM rows", [], |r| r.get(0))?;
        Ok(Store {
            conn,
            view: View::All(n as u64),
        })
    }

    pub fn clear(&mut self) -> Result<()> {
        self.conn
            .execute_batch("DELETE FROM rows; DELETE FROM sources;")?;
        self.view = View::All(0);
        Ok(())
    }

    fn source_id(&self, path: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT OR IGNORE INTO sources (path) VALUES (?1)",
            params![path],
        )?;
        Ok(self
            .conn
            .query_row("SELECT id FROM sources WHERE path=?1", params![path], |r| {
                r.get(0)
            })?)
    }

    pub fn total(&self) -> Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM rows", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Add `n` synthetic rows, for exercising the grid at scale.
    pub fn add_synthetic(&mut self, n: u64) -> Result<()> {
        let src = self.source_id("synthetic")?;
        let exts = ["jpg", "png", "mp4", "pdf", "docx", "sqlite", "txt", "zip"];
        let bands = ["GREEN", "YELLOW", "RED"];
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO rows (kind, source_id, name, path, ext, size, band, score, status,
                                   offset, length, reasons, modified)
                 VALUES ('synthetic', ?1, ?2, ?3, ?4, ?5, ?6, ?7, 'synthetic', ?8, ?5, '', ?9)",
            )?;
            let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
            for i in 0..n {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let ext = exts[(x % 8) as usize];
                let name = format!("file_{i:08}.{ext}");
                let path = format!("dir_{:04}/{name}", (x >> 8) % 5000);
                let score = (x >> 20) % 101;
                let band = bands[if score >= 90 {
                    0
                } else if score >= 45 {
                    1
                } else {
                    2
                }];
                st.execute(params![
                    src,
                    name,
                    path,
                    ext,
                    ((x >> 30) % (64 << 20)) as i64,
                    band,
                    score as i64,
                    (i * 4096) as i64,
                    1_600_000_000 + ((x >> 12) % 100_000_000) as i64,
                ])?;
            }
        }
        tx.commit()?;
        self.view = View::All(self.total()?);
        Ok(())
    }

    /// Apply a filter and sort; returns the number of rows in the view.
    pub fn set_view(&mut self, f: &Filter) -> Result<u64> {
        let max: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM rows", [], |r| r.get(0))?;
        if f.is_identity() && max as u64 == self.total()? {
            self.view = View::All(max as u64);
            return Ok(max as u64);
        }
        let mut sql = String::from("SELECT id FROM rows WHERE 1=1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !f.text.is_empty() {
            sql.push_str(" AND instr(lower(path), ?) > 0");
            args.push(f.text.to_lowercase().into());
        }
        let folder = normalise_folder(&f.folder);
        if !folder.is_empty() {
            // A prefix compare rather than LIKE, so `_` and `%` in folder
            // names mean themselves; kind = 'deleted' because a carved row's
            // path is only its extension directory.
            sql.push_str(" AND kind = 'deleted' AND substr(lower(path), 1, ?) = ?");
            args.push((folder.chars().count() as i64).into());
            args.push(folder.into());
        }
        for (col, vals) in [("band", &f.bands), ("kind", &f.kinds), ("ext", &f.exts)] {
            if !vals.is_empty() {
                sql.push_str(&format!(
                    " AND {col} IN ({})",
                    vec!["?"; vals.len()].join(",")
                ));
                args.extend(vals.iter().map(|v| rusqlite::types::Value::from(v.clone())));
            }
        }
        if let Some(m) = f.min_size {
            sql.push_str(" AND size >= ?");
            args.push((m as i64).into());
        }
        if let Some(col) = &f.sort {
            let col = match col.as_str() {
                "name" | "path" | "ext" | "size" | "band" | "score" | "modified" | "kind" => col,
                _ => "id",
            };
            sql.push_str(&format!(
                " ORDER BY {col} {}, id",
                if f.descending { "DESC" } else { "ASC" }
            ));
        }
        let mut st = self.conn.prepare(&sql)?;
        let ids: Vec<u32> = st
            .query_map(rusqlite::params_from_iter(args), |r| r.get::<_, i64>(0))?
            .map(|r| r.map(|v| v as u32))
            .collect::<std::result::Result<_, _>>()?;
        let n = ids.len() as u64;
        self.view = View::Ids(ids);
        Ok(n)
    }

    pub fn view_len(&self) -> u64 {
        match &self.view {
            View::All(n) => *n,
            View::Ids(v) => v.len() as u64,
        }
    }

    /// Rows at positions `start..start+count` of the view.
    pub fn rows(&self, start: u64, count: u64) -> Result<Vec<Row>> {
        let ids: Vec<i64> = match &self.view {
            View::All(n) => (start..(start + count).min(*n))
                .map(|i| i as i64 + 1)
                .collect(),
            View::Ids(v) => v
                .iter()
                .skip(start as usize)
                .take(count as usize)
                .map(|&i| i as i64)
                .collect(),
        };
        let mut st = self.conn.prepare_cached(
            "SELECT r.id, kind, s.path, name, r.path, ext, size, band, score, status, offset,
                    reasons, modified
             FROM rows r JOIN sources s ON s.id = r.source_id WHERE r.id = ?1",
        )?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = st
                .query_row(params![id], |r| {
                    Ok(Row {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        source: r.get(2)?,
                        name: r.get(3)?,
                        path: r.get(4)?,
                        ext: r.get(5)?,
                        size: r.get::<_, i64>(6)? as u64,
                        band: r.get(7)?,
                        score: r.get(8)?,
                        status: r.get(9)?,
                        offset: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
                        reasons: r.get(11)?,
                        modified: r.get(12)?,
                    })
                })
                .optional()?
            {
                out.push(row);
            }
        }
        Ok(out)
    }

    fn detail(&self, id: i64) -> Result<RowDetail> {
        let (source, offset, length, detail): (String, Option<i64>, Option<i64>, Option<String>) =
            self.conn.query_row(
                "SELECT s.path, offset, length, detail FROM rows r JOIN sources s
                 ON s.id = r.source_id WHERE r.id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        let detail = detail.and_then(|d| serde_json::from_str(&d).ok());
        Ok((
            source,
            offset.map(|v| v as u64),
            length.map(|v| v as u64),
            detail,
        ))
    }
}

/// A row's source device, content offset and length, and what kind of row it
/// is: everything the operations below need that the grid does not show.
type RowDetail = (String, Option<u64>, Option<u64>, Option<Detail>);

/// `Photos\2024\` or `/photos/2024` -> `photos/2024/`: lower case, `/`
/// separators, no leading slash, one trailing slash so `photos/2024` does not
/// also match `photos/20245`. Empty for the volume's root.
fn normalise_folder(folder: &str) -> String {
    let parts: Vec<String> = folder
        .split(['/', '\\'])
        .filter(|p| !p.is_empty() && *p != ".")
        .map(|p| p.to_lowercase())
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("{}/", parts.join("/"))
    }
}

/// Progress of a scan, for the GUI's progress bar. `total` is 0 when the size
/// of the work is not known (a filesystem scan): the bar is then shown as
/// indeterminate rather than invented.
#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    pub phase: String,
    pub done: u64,
    pub total: u64,
    pub found: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScanSummary {
    pub rows_added: u64,
    pub notes: Vec<String>,
}

/// A row found by a scan, not yet in the store. Scans produce these without
/// touching the store, so the grid stays usable while they run; the rows are
/// added in one short transaction at the end with [`Store::add`].
pub struct Pending {
    kind: &'static str,
    name: String,
    path: String,
    ext: String,
    size: u64,
    band: String,
    score: i64,
    status: String,
    offset: Option<u64>,
    reasons: String,
    modified: Option<i64>,
    detail: Detail,
}

/// What a scan found.
pub struct Found {
    pub source: PathBuf,
    pub rows: Vec<Pending>,
    pub notes: Vec<String>,
}

impl Store {
    /// Add a scan's rows. Returns how many.
    pub fn add(&mut self, found: Found) -> Result<ScanSummary> {
        let src = self.source_id(&found.source.to_string_lossy())?;
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO rows (kind, source_id, name, path, ext, size, band, score, status,
                                   offset, length, reasons, modified, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?6, ?11, ?12, ?13)",
            )?;
            for p in &found.rows {
                st.execute(params![
                    p.kind,
                    src,
                    p.name,
                    p.path,
                    p.ext,
                    p.size as i64,
                    p.band,
                    p.score,
                    p.status,
                    p.offset.map(|o| o as i64),
                    p.reasons,
                    p.modified,
                    serde_json::to_string(&p.detail).map_err(|e| Error::Other(e.to_string()))?,
                ])?;
            }
        }
        tx.commit()?;
        self.view = View::All(self.total()?);
        Ok(ScanSummary {
            rows_added: found.rows.len() as u64,
            notes: found.notes,
        })
    }
}

/// Scan every supported filesystem on `source` for deleted files and rate
/// them. Unsupported filesystems are named in the notes, not silently skipped.
pub fn scan_filesystems(
    source: &Path,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<Found> {
    let device = rc_device::open(source, None)?;
    let mut table = rc_partition::discover(device.as_ref(), &Default::default())?;
    rc_partition::refine_filesystems(device.as_ref(), &mut table);
    let ss = device.sector_size();
    let mut notes = Vec::new();
    let mut offsets: Vec<u64> = Vec::new();
    for p in &table.partitions {
        if p.fs.is_supported() {
            offsets.push(p.byte_offset(ss));
        } else {
            notes.push(format!(
                "partition {} ({}) skipped: {}",
                p.index,
                p.fs,
                match p.fs {
                    rc_partition::FsKind::Apfs | rc_partition::FsKind::HfsPlus =>
                        "detection only; deleted-file recovery not implemented - carve instead",
                    _ => "not a filesystem this tool parses",
                }
            ));
        }
    }
    if offsets.is_empty() {
        offsets.push(0);
    }
    let rules = Rules::builtin();
    let mut rows = Vec::new();
    for (k, offset) in offsets.iter().enumerate() {
        progress(Progress {
            phase: format!("reading filesystem metadata ({}/{})", k + 1, offsets.len()),
            done: 0,
            total: 0,
            found: rows.len() as u64,
        });
        let (fs, scan) = match rc_fs::scan_volume(device.as_ref(), *offset) {
            Ok(x) => x,
            Err(e) => {
                notes.push(format!("volume at byte {offset}: {e}"));
                continue;
            }
        };
        notes.extend(scan.notes.iter().cloned());
        let occupancy = Occupancy::from_scan(&scan);
        let ctx = Context::new(device.as_ref(), &scan, &occupancy, &rules, fs.to_string());
        let deleted: Vec<&Entry> = scan
            .deleted()
            .filter(|e| e.kind == rc_fs::EntryKind::File)
            .collect();
        let g: Geometry = scan.geometry;
        for (i, e) in deleted.iter().enumerate() {
            if stop.load(Ordering::Relaxed) {
                notes.push("stopped; the files rated so far are listed".into());
                break;
            }
            let score = score_entry(&ctx, e)?;
            let offset = match &e.location {
                DataLocation::Runs(r) => r
                    .iter()
                    .find(|x| !x.sparse)
                    .and_then(|x| g.cluster_offset(x.start_cluster)),
                DataLocation::FirstClusterOnly(c) => g.cluster_offset(*c),
                _ => None,
            };
            rows.push(Pending {
                kind: "deleted",
                name: e.name.clone(),
                path: e.display_path(),
                ext: ext_of(&e.name),
                size: e.size,
                band: score.band.to_string(),
                score: score.value as i64,
                status: format!("{:?}", e.path_confidence)
                    .split([' ', '{', '('])
                    .next()
                    .unwrap_or("")
                    .to_string(),
                offset,
                reasons: score
                    .reasons
                    .iter()
                    .map(|r| format!("{}: {}", r.rule, r.detail))
                    .collect::<Vec<_>>()
                    .join("\n"),
                modified: e.timestamps.modified.map(|t| t / 1_000_000_000),
                detail: Detail::Deleted {
                    entry: Box::new((*e).clone()),
                    cluster_bytes: g.cluster_bytes,
                    heap_offset: g.heap_offset,
                    first_cluster: g.first_cluster,
                    cluster_count: g.cluster_count,
                },
            });
            if i % 256 == 0 {
                progress(Progress {
                    phase: format!("rating deleted files on {fs}"),
                    done: i as u64,
                    total: deleted.len() as u64,
                    found: rows.len() as u64,
                });
            }
        }
    }
    Ok(Found {
        source: source.to_path_buf(),
        rows,
        notes,
    })
}

/// Carve `source` by signature into `index` (an `rc-session` carve, so it can
/// be stopped and its candidates so far still listed).
pub fn carve(
    source: &Path,
    index: &Path,
    stop: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<Found> {
    let device: Arc<dyn rc_device::ReadOnlyDevice> = Arc::from(rc_device::open(source, None)?);
    let db = rc_carve::SignatureDb::builtin()?;
    let settings = rc_session::Settings::default();
    let end = device.total_bytes();
    let (outcome, _totals) = rc_session::start(
        device.clone(),
        source,
        &db,
        &settings,
        (0, end),
        index,
        stop,
        &mut |p| {
            progress(Progress {
                phase: "carving".into(),
                done: p.next_offset - p.range_start,
                total: p.range_end - p.range_start,
                found: p.candidates,
            })
        },
    )?;
    let mut notes = Vec::new();
    if let rc_session::Outcome::Stopped { next_offset, .. } = outcome {
        notes.push(format!(
            "stopped at byte {next_offset}; the candidates so far are listed"
        ));
    }
    let ix = rc_index::CandidateIndex::open(index)?;
    let rows = ix
        .page(0, usize::MAX)?
        .into_iter()
        .map(|c| {
            let name = format!("{:012x}.{}", c.offset, c.ext);
            Pending {
                kind: "carved",
                path: format!("carved/{}/{name}", c.ext),
                name,
                ext: c.ext.clone(),
                size: c.length,
                band: String::new(),
                score: 0,
                status: c.status.clone(),
                offset: Some(c.offset),
                reasons: c.detail.clone(),
                modified: None,
                detail: Detail::Carved {
                    validator: db.get(&c.signature_id).and_then(|s| s.validator.clone()),
                    signature_id: c.signature_id.clone(),
                },
            }
        })
        .collect();
    Ok(Found {
        source: source.to_path_buf(),
        rows,
        notes,
    })
}

fn geometry(
    cluster_bytes: u64,
    heap_offset: u64,
    first_cluster: u64,
    cluster_count: u64,
) -> Geometry {
    Geometry {
        cluster_bytes,
        heap_offset,
        first_cluster,
        cluster_count,
    }
}

/// Up to `max` bytes of a row's content, read the way restoring would.
pub fn read_content(store: &Store, id: i64, max: u64) -> Result<Vec<u8>> {
    let (source, offset, length, detail) = store.detail(id)?;
    let dev = rc_device::open(Path::new(&source), None)?;
    match detail {
        Some(Detail::Deleted {
            entry,
            cluster_bytes,
            heap_offset,
            first_cluster,
            cluster_count,
        }) => {
            let g = geometry(cluster_bytes, heap_offset, first_cluster, cluster_count);
            let want = entry.size.min(max) as usize;
            let mut out = Vec::with_capacity(want);
            match &entry.location {
                DataLocation::Resident(b) => out.extend_from_slice(&b[..want.min(b.len())]),
                DataLocation::Runs(runs) => {
                    for r in runs {
                        let len = (r.cluster_count * g.cluster_bytes) as usize;
                        let take = len.min(want - out.len());
                        if r.sparse {
                            out.resize(out.len() + take, 0);
                        } else if let Some(at) = g.cluster_offset(r.start_cluster) {
                            let mut buf = vec![0u8; take];
                            let n = dev.read_bytes_at(at, &mut buf)?;
                            out.extend_from_slice(&buf[..n]);
                        }
                        if out.len() >= want {
                            break;
                        }
                    }
                }
                DataLocation::FirstClusterOnly(c) => {
                    if let Some(at) = g.cluster_offset(*c) {
                        let mut buf = vec![0u8; want];
                        let n = dev.read_bytes_at(at, &mut buf)?;
                        out.extend_from_slice(&buf[..n]);
                    }
                }
                DataLocation::Unknown => {
                    return Err(Error::Other(
                        "where this file's content was is not known".into(),
                    ))
                }
            }
            Ok(out)
        }
        _ => {
            let (Some(offset), Some(length)) = (offset, length) else {
                return Err(Error::Other("this row has no content location".into()));
            };
            let mut buf = vec![0u8; length.min(max) as usize];
            let n = dev.read_bytes_at(offset, &mut buf)?;
            buf.truncate(n);
            Ok(buf)
        }
    }
}

/// A PNG preview of a row: images decoded in memory, video through ffmpeg over
/// pipes. No temp files (rc-preview).
pub fn preview(store: &Store, id: i64, max_dim: u32) -> Result<Vec<u8>> {
    let row = store
        .conn
        .query_row("SELECT ext FROM rows WHERE id=?1", params![id], |r| {
            r.get::<_, String>(0)
        })?;
    let bytes = read_content(store, id, 256 << 20)?;
    if matches!(
        row.as_str(),
        "mp4" | "mov" | "m4v" | "3gp" | "avi" | "mkv" | "webm"
    ) {
        let mut opts = rc_preview::FfmpegOptions::find().ok_or_else(|| {
            Error::Other(
            "ffmpeg was not found. Video previews need it: put ffmpeg.exe beside rc.exe \n             or anywhere on PATH."
                .into(),
        )
        })?;
        opts.max_dim = max_dim;
        return Ok(rc_preview::video_frame(&bytes, &opts)?);
    }
    Ok(rc_preview::thumbnail(&bytes, max_dim)?.png)
}

/// Raw device bytes around a row's content start, for the hex viewer.
pub fn hex(store: &Store, id: i64, skip: u64, len: u64) -> Result<rc_preview::HexView> {
    let (source, offset, _, _) = store.detail(id)?;
    let at = offset.ok_or_else(|| Error::Other("this row has no device offset".into()))?;
    let dev = rc_device::open(Path::new(&source), None)?;
    Ok(rc_preview::hex_view(
        dev.as_ref(),
        at + skip,
        len.min(1 << 16),
        &[],
    )?)
}

/// Write rows' content under `out`, through the sink that refuses the device
/// being read. Carved rows that did not validate are reassembled when
/// `reassemble` is set and their format has a validator.
pub fn restore(
    store: &Store,
    ids: &[i64],
    out: &Path,
    reassemble: bool,
) -> Result<Vec<rc_restore::Restored>> {
    let mut r = rc_restore::Restorer::new(out)?;
    let mut devices: std::collections::HashMap<String, Box<dyn rc_device::ReadOnlyDevice>> =
        Default::default();
    for &id in ids {
        let (source, offset, length, detail) = store.detail(id)?;
        if !devices.contains_key(&source) {
            devices.insert(source.clone(), rc_device::open(Path::new(&source), None)?);
        }
        let dev = devices[&source].as_ref();
        let (path, status, reasons): (String, String, String) = store.conn.query_row(
            "SELECT path, status, reasons FROM rows WHERE id=?1",
            params![id],
            |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)),
        )?;
        match detail {
            Some(Detail::Deleted {
                entry,
                cluster_bytes,
                heap_offset,
                first_cluster,
                cluster_count,
            }) => {
                let g = geometry(cluster_bytes, heap_offset, first_cluster, cluster_count);
                r.entry(
                    dev,
                    &g,
                    &entry,
                    reasons.lines().map(str::to_string).collect(),
                )?;
            }
            Some(Detail::Carved {
                validator,
                signature_id,
            }) => {
                let (Some(offset), Some(length)) = (offset, length) else {
                    continue;
                };
                if status != "valid" && reassemble {
                    if let Some(v) = validator {
                        let a = rc_bifrag::reassemble(dev, &v, offset, &Default::default())
                            .map_err(|e| Error::Other(e.to_string()))?;
                        if a.status == rc_carve::Status::Valid {
                            let spans: Vec<rc_restore::Span> = a
                                .pieces
                                .iter()
                                .map(|p| rc_restore::Span {
                                    offset: p.offset,
                                    length: p.length,
                                })
                                .collect();
                            r.spans(
                                dev,
                                &path,
                                &spans,
                                a.length,
                                rc_restore::Layout::Reassembled,
                                format!("carved at byte {offset} ({signature_id})"),
                                vec![format!(
                                    "{} fragments; validated after reassembly",
                                    a.fragments()
                                )],
                            )?;
                            continue;
                        }
                    }
                }
                r.spans(
                    dev,
                    &path,
                    &[rc_restore::Span { offset, length }],
                    length,
                    rc_restore::Layout::Carved,
                    format!("carved at byte {offset} ({signature_id})"),
                    vec![format!("status: {status}")],
                )?;
            }
            None => {}
        }
    }
    Ok(r.finish()?)
}

/// Where the GUI keeps its results database: beside the executable's data,
/// never on a scanned device (checked by the sink on every restore, and the
/// database itself is only ever opened on the system's local app data).
pub fn default_store_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("XDG_DATA_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("recovery-core").join("results.sqlite")
}

pub fn stop_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
