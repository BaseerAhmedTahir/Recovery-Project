//! `rc-index` - the candidate index, on disk rather than in memory.
//!
//! A carve of a real drive produces far more candidates than a carve of a
//! fixture. The quick-formatted 512 MiB fixture yields 4203 header matches;
//! at the same rate a 4 TB drive yields roughly 34 million, and that is before
//! the free space of a used disk contributes its own noise. Holding those in a
//! `Vec` is what makes a carving tool die two hours into an overnight run.
//!
//! So SPEC.md section 5.8 asks for a disk-backed SQLite index in a scratch
//! directory, with peak RSS under ~2 GB regardless of drive size. This is that
//! index.
//!
//! # The claim, and how it is checked
//!
//! "Disk-backed" is easy to write and easy to get wrong: an index that batches
//! into a growing `Vec` before every commit is disk-backed in name and
//! resident in fact. Reading the code does not settle it, so the test grows the
//! candidate count tenfold and measures the peak.
//!
//! The pass condition is **sublinear with a named ratio**: ten times the
//! candidates must cost no more than twice the peak memory. "Flat" would be
//! the wrong bar - SQLite's page cache, the WAL and the connection itself all
//! grow a little - and an unnamed bar is one that gets argued down when it
//! fails.
//!
//! # Why not just stream to a file
//!
//! Because the index is read back non-sequentially. Milestone 5 scores
//! candidates by whether their extents overlap clusters that live files
//! occupy, the GUI pages through ten million rows by offset or by type, and
//! `rc-session` resumes a scan by asking what was already found beyond a given
//! LBA. Those are queries, and SQLite is the answer SPEC.md already chose.

pub mod rss;

use rc_carve::scan::Candidate;
use rc_carve::validate::Status;
use rc_carve::Category;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{path}: {detail}")]
    Io { path: String, detail: String },
}

pub type Result<T> = std::result::Result<T, IndexError>;

/// How many rows to accumulate before committing.
///
/// This is the one number that decides whether the index is really on disk.
/// Every row held here is resident, so the batch is a memory bound - 4096 rows
/// is a few hundred kilobytes and amortises the transaction cost, while
/// committing per row would be correct and unusably slow.
const BATCH: usize = 4096;

/// SQLite page cache, in KiB.
///
/// This is the index's real memory bound and it was set by measurement, not by
/// taste. At SQLite's default the cache is the thing that grows with the scan:
/// indexing 50,000 candidates cost 8.3 MiB of peak RSS and 500,000 cost 26.2
/// MiB, which is not the index holding rows - it is the cache filling up - but
/// it still fails a "10x the candidates, no more than 2x the memory" bar,
/// because the smaller run never reached the plateau.
///
/// 8 MiB is reached early enough that both runs sit at it, and a carve is
/// write-heavy anyway: pages are appended through the WAL and rarely read back
/// during the scan itself, so a large cache buys little. Raise it for a
/// read-heavy workload - paging a finished index in the GUI - where the
/// trade-off runs the other way.
const PAGE_CACHE_KIB: i64 = 8_000;

pub struct CandidateIndex {
    conn: Connection,
    pending: Vec<Candidate>,
    written: u64,
}

impl CandidateIndex {
    /// Create a new index at `path`, replacing any existing one.
    pub fn create(path: &Path) -> Result<CandidateIndex> {
        if path.exists() {
            std::fs::remove_file(path).map_err(|e| IndexError::Io {
                path: path.display().to_string(),
                detail: format!("removing the previous index: {e}"),
            })?;
        }
        let conn = Connection::open(path)?;
        Self::configure(&conn)?;
        Self::schema(&conn)?;
        Ok(CandidateIndex {
            conn,
            pending: Vec::with_capacity(BATCH),
            written: 0,
        })
    }

    /// An in-memory index. For tests that are about behaviour rather than
    /// residency - the memory tests use `create` against a real file, since an
    /// in-memory database would make the measurement meaningless by
    /// construction.
    pub fn in_memory() -> Result<CandidateIndex> {
        let conn = Connection::open_in_memory()?;
        Self::schema(&conn)?;
        Ok(CandidateIndex {
            conn,
            pending: Vec::with_capacity(BATCH),
            written: 0,
        })
    }

    pub fn open(path: &Path) -> Result<CandidateIndex> {
        let conn = Connection::open(path)?;
        Self::configure(&conn)?;
        Ok(CandidateIndex {
            conn,
            pending: Vec::with_capacity(BATCH),
            written: 0,
        })
    }

    fn configure(conn: &Connection) -> Result<()> {
        // WAL as SPEC.md section 5.8 specifies: a reader can page through
        // results while the scan is still writing, which is what the GUI needs.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // The scan can be killed at any moment and the index rebuilt, so
        // durability is worth nothing here and fsync-per-commit costs a lot.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Cap the page cache explicitly. A negative value means KiB rather
        // than pages, and leaving it at the default is one of the ways a
        // "disk-backed" index quietly becomes a resident one. See
        // PAGE_CACHE_KIB for the measurement behind the number.
        conn.pragma_update(None, "cache_size", -PAGE_CACHE_KIB)?;
        // Sorts and temporary B-trees go to a file rather than to the heap.
        conn.pragma_update(None, "temp_store", "FILE")?;
        // Checkpoint the WAL roughly every 4 MiB. This is SQLite's default and
        // is stated rather than inherited, because an unbounded WAL takes its
        // shared-memory index up with it and that is resident.
        conn.pragma_update(None, "wal_autocheckpoint", 1000)?;
        Ok(())
    }

    fn schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS candidates (
                 id                 INTEGER PRIMARY KEY,
                 offset             INTEGER NOT NULL,
                 length             INTEGER NOT NULL,
                 signature_id       TEXT    NOT NULL,
                 ext                TEXT    NOT NULL,
                 category           TEXT    NOT NULL,
                 status             TEXT    NOT NULL,
                 length_established INTEGER NOT NULL,
                 detail             TEXT    NOT NULL
             );
             CREATE TABLE IF NOT EXISTS evidence (
                 candidate_id INTEGER NOT NULL,
                 key          TEXT    NOT NULL,
                 value        TEXT    NOT NULL
             );",
        )?;
        Ok(())
    }

    /// Build the indexes.
    ///
    /// Deliberately deferred until after the rows are in: maintaining a B-tree
    /// per insert costs more than building it once at the end, and the scan is
    /// the part that has to keep up with a device.
    pub fn finish(&mut self) -> Result<()> {
        self.flush()?;
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_candidates_offset ON candidates(offset);
             CREATE INDEX IF NOT EXISTS idx_candidates_ext    ON candidates(ext);
             CREATE INDEX IF NOT EXISTS idx_candidates_status ON candidates(status);
             CREATE INDEX IF NOT EXISTS idx_evidence_id       ON evidence(candidate_id);",
        )?;
        Ok(())
    }

    pub fn push(&mut self, c: Candidate) -> Result<()> {
        self.pending.push(c);
        if self.pending.len() >= BATCH {
            self.flush()?;
        }
        Ok(())
    }

    pub fn extend(&mut self, it: impl IntoIterator<Item = Candidate>) -> Result<()> {
        for c in it {
            self.push(c)?;
        }
        Ok(())
    }

    /// Write everything buffered. Called automatically once a batch fills.
    pub fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO candidates
                   (offset, length, signature_id, ext, category, status,
                    length_established, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            let mut ins_ev = tx.prepare_cached(
                "INSERT INTO evidence (candidate_id, key, value) VALUES (?1, ?2, ?3)",
            )?;
            for c in &self.pending {
                ins.execute(params![
                    c.offset as i64,
                    c.length as i64,
                    c.signature_id,
                    c.ext,
                    c.category.as_str(),
                    status_str(c.status),
                    c.length_established as i32,
                    c.detail,
                ])?;
                let id = tx.last_insert_rowid();
                for (k, v) in &c.evidence {
                    ins_ev.execute(params![id, k, v])?;
                }
            }
        }
        tx.commit()?;
        self.written += self.pending.len() as u64;
        // `clear` keeps the allocation, which is the point: the buffer is
        // reused for the next batch rather than regrown.
        self.pending.clear();
        Ok(())
    }

    /// Rows committed so far. Does not include anything still buffered.
    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn count(&self) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM candidates", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    pub fn count_by_ext(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT ext, COUNT(*) FROM candidates GROUP BY ext ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One page of results in device order, which is how the GUI reads them.
    pub fn page(&self, offset_from: u64, limit: usize) -> Result<Vec<IndexedCandidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, offset, length, signature_id, ext, category, status,
                    length_established, detail
             FROM candidates WHERE offset >= ?1 ORDER BY offset, id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![offset_from as i64, limit as i64], row_to_candidate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn by_ext(&self, ext: &str, limit: usize) -> Result<Vec<IndexedCandidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, offset, length, signature_id, ext, category, status,
                    length_established, detail
             FROM candidates WHERE ext = ?1 ORDER BY offset LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![ext, limit as i64], row_to_candidate)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The evidence vector for one candidate, so the UI can explain a rating
    /// without re-reading the device.
    pub fn evidence(&self, id: i64) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM evidence WHERE candidate_id = ?1")?;
        let rows = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn at_offset(&self, offset: u64) -> Result<Option<IndexedCandidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, offset, length, signature_id, ext, category, status,
                    length_established, detail
             FROM candidates WHERE offset = ?1 LIMIT 1",
        )?;
        Ok(stmt
            .query_row(params![offset as i64], row_to_candidate)
            .optional()?)
    }
}

fn row_to_candidate(r: &rusqlite::Row) -> rusqlite::Result<IndexedCandidate> {
    Ok(IndexedCandidate {
        id: r.get(0)?,
        offset: r.get::<_, i64>(1)? as u64,
        length: r.get::<_, i64>(2)? as u64,
        signature_id: r.get(3)?,
        ext: r.get(4)?,
        category: r.get(5)?,
        status: r.get(6)?,
        length_established: r.get::<_, i32>(7)? != 0,
        detail: r.get(8)?,
    })
}

/// A candidate read back out of the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedCandidate {
    pub id: i64,
    pub offset: u64,
    pub length: u64,
    pub signature_id: String,
    pub ext: String,
    pub category: String,
    pub status: String,
    pub length_established: bool,
    pub detail: String,
}

fn status_str(s: Status) -> &'static str {
    match s {
        Status::Valid => "valid",
        Status::Partial => "partial",
        Status::Rejected => "rejected",
    }
}

/// Category names, kept next to the index so a schema reader does not have to
/// go looking in `rc-carve`.
pub fn category_str(c: Category) -> &'static str {
    c.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(offset: u64, ext: &str) -> Candidate {
        Candidate {
            offset,
            length: 4096,
            signature_id: ext.to_string(),
            ext: ext.to_string(),
            category: Category::Image,
            status: Status::Valid,
            detail: String::new(),
            evidence: vec![("width", "640".to_string()), ("height", "480".to_string())],
            length_established: true,
            header_span: 8,
        }
    }

    #[test]
    fn round_trips_a_candidate_with_its_evidence() {
        let mut ix = CandidateIndex::in_memory().expect("index");
        ix.push(candidate(8192, "jpg")).expect("push");
        ix.finish().expect("finish");

        assert_eq!(ix.count().expect("count"), 1);
        let got = ix.at_offset(8192).expect("query").expect("row");
        assert_eq!(got.offset, 8192);
        assert_eq!(got.ext, "jpg");
        assert_eq!(got.status, "valid");
        assert!(got.length_established);

        let ev = ix.evidence(got.id).expect("evidence");
        assert_eq!(ev.len(), 2);
        assert!(ev.iter().any(|(k, v)| k == "width" && v == "640"));
    }

    #[test]
    fn pages_in_device_order() {
        let mut ix = CandidateIndex::in_memory().expect("index");
        // Insert deliberately out of order.
        for off in [40_000u64, 10_000, 30_000, 20_000] {
            ix.push(candidate(off, "png")).expect("push");
        }
        ix.finish().expect("finish");

        let page = ix.page(0, 10).expect("page");
        let offsets: Vec<u64> = page.iter().map(|c| c.offset).collect();
        assert_eq!(offsets, vec![10_000, 20_000, 30_000, 40_000]);

        let page = ix.page(20_000, 2).expect("page");
        assert_eq!(
            page.iter().map(|c| c.offset).collect::<Vec<_>>(),
            vec![20_000, 30_000]
        );
    }

    #[test]
    fn counts_by_extension() {
        let mut ix = CandidateIndex::in_memory().expect("index");
        for i in 0..5 {
            ix.push(candidate(i * 100, "jpg")).expect("push");
        }
        for i in 0..3 {
            ix.push(candidate(10_000 + i * 100, "png")).expect("push");
        }
        ix.finish().expect("finish");

        let counts = ix.count_by_ext().expect("counts");
        assert_eq!(counts[0], ("jpg".to_string(), 5));
        assert_eq!(counts[1], ("png".to_string(), 3));
    }

    /// Rows buffered but not yet committed must not be visible or lost.
    #[test]
    fn a_partial_batch_is_written_by_finish() {
        let mut ix = CandidateIndex::in_memory().expect("index");
        for i in 0..10u64 {
            ix.push(candidate(i * 512, "pdf")).expect("push");
        }
        assert_eq!(ix.written(), 0, "nothing should be committed yet");
        assert_eq!(ix.count().expect("count"), 0);

        ix.finish().expect("finish");
        assert_eq!(ix.written(), 10);
        assert_eq!(ix.count().expect("count"), 10);
    }

    #[test]
    fn a_full_batch_commits_on_its_own() {
        let mut ix = CandidateIndex::in_memory().expect("index");
        for i in 0..(BATCH as u64 + 5) {
            ix.push(candidate(i * 16, "bin")).expect("push");
        }
        assert_eq!(
            ix.written(),
            BATCH as u64,
            "one full batch should have committed without finish()"
        );
        ix.finish().expect("finish");
        assert_eq!(ix.count().expect("count"), BATCH as u64 + 5);
    }

    #[test]
    fn survives_being_closed_and_reopened() {
        let dir = std::env::temp_dir().join(format!("rc-index-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("reopen.db");
        {
            let mut ix = CandidateIndex::create(&path).expect("create");
            for i in 0..100u64 {
                ix.push(candidate(i * 4096, "jpg")).expect("push");
            }
            ix.finish().expect("finish");
        }
        let ix = CandidateIndex::open(&path).expect("open");
        assert_eq!(ix.count().expect("count"), 100);
        assert_eq!(ix.by_ext("jpg", 5).expect("by_ext").len(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
