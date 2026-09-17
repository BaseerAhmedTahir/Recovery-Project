//! Recover deleted rows from SQLite databases (SPEC.md 1.2, 6.1, Milestone 7).
//!
//! # Where deleted rows survive
//!
//! With `secure_delete` off (the SQLite default, though not every build's), a
//! `DELETE` does not erase the row's bytes:
//!
//! - **Freeblocks.** A deleted cell's space inside a page that stays in use
//!   joins the page's freeblock chain. The first four bytes are overwritten
//!   with the chain link and size, which usually destroys the cell's payload
//!   length and rowid; the record header and values after them survive.
//! - **Unallocated space** between a page's cell pointer array and its cell
//!   content area.
//! - **Freelist pages.** A page emptied entirely goes onto the freelist. A
//!   freelist leaf page is not rewritten, so the whole old page is intact,
//!   rowids and all; a trunk page loses its first bytes to the page list.
//! - **Older page versions in a WAL.** In WAL mode a change writes a new copy
//!   of the page to the `-wal` file; the main file keeps the old copy until a
//!   checkpoint, and earlier frames in the WAL keep earlier copies. Rows that
//!   are not in the current version of the database but are in an older copy
//!   are deleted rows.
//!
//! # How they are found
//!
//! The database is read from bytes and never opened through SQLite (see
//! `format.rs`). The current state is built in memory - main file overlaid with
//! committed WAL frames - and walked to get the schema and every live row.
//! Then every place a deleted row can be is searched for records that fit a
//! table's schema: exactly its column count, a NULL where the rowid alias
//! column is, and serial types its declared column types can hold. A candidate
//! equal to a live row is a stale copy of it, not a deleted row, and is dropped.
//!
//! # What it misses
//!
//! - Rows whose space was reused by later writes. Nothing to find.
//! - Rows whose payload spilled onto overflow pages: only records that fit on
//!   their page are carved from free space.
//! - Rows in freeblocks whose first four bytes reached past the record's
//!   header-size byte into its serial types, beyond a leading rowid alias
//!   column (whose type can only have been NULL and is restored).
//! - `WITHOUT ROWID` tables, rollback journals, and `secure_delete=ON`
//!   databases (zeroed: nothing to recover, and nothing is invented).

pub mod format;

use format::{be16, be32, decode_record, local_payload, serial_len, varint, Value, HEADER_MAGIC};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a SQLite database: {0}")]
    NotSqlite(String),
    #[error("no table named {0}")]
    NoSuchTable(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, Serialize)]
pub struct Column {
    pub name: String,
    pub decl_type: String,
    /// `INTEGER PRIMARY KEY`: stored as NULL in the record, the value is the
    /// rowid.
    pub rowid_alias: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Table {
    pub name: String,
    pub root_page: u32,
    pub columns: Vec<Column>,
    pub live_rows: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// A freeblock inside a page that is still in use.
    Freeblock { page: u32 },
    /// Unallocated space inside a page that is still in use.
    UnallocatedSpace { page: u32 },
    /// A page on the freelist.
    FreelistPage { page: u32 },
    /// A page nothing in the database refers to.
    UnreferencedPage { page: u32 },
    /// A copy of a page that a later version replaced: in the main file, when a
    /// WAL holds a newer one, or in an earlier WAL frame.
    OlderPageVersion { page: u32, wal_frame: Option<u32> },
    /// A WAL frame after the last commit: a transaction that never completed.
    UncommittedWalFrame { page: u32, wal_frame: u32 },
}

#[derive(Clone, Debug, Serialize)]
pub struct RecoveredRow {
    pub table: String,
    /// Known when the cell header survived; freeblocks usually overwrite it.
    pub rowid: Option<i64>,
    pub values: Vec<Value>,
    pub source: Source,
    /// `"db"` or `"wal"`, and the byte offset of the record in that file.
    pub file: &'static str,
    pub offset: u64,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub page_size: usize,
    pub pages: u32,
    pub wal_frames_committed: u32,
    pub wal_frames_uncommitted: u32,
    pub tables: Vec<Table>,
    pub recovered: Vec<RecoveredRow>,
    pub notes: Vec<String>,
}

/// One page image and where it came from.
#[derive(Clone, Copy)]
struct PageRef<'a> {
    number: u32,
    data: &'a [u8],
    file: &'static str,
    file_offset: u64,
}

struct Header {
    page_size: usize,
    usable: usize,
    encoding: u32,
}

fn parse_header(db: &[u8]) -> Result<Header> {
    if db.len() < 100 || &db[..16] != HEADER_MAGIC {
        return Err(Error::NotSqlite("no 'SQLite format 3' header".into()));
    }
    let raw = be16(db, 16).unwrap_or(0);
    let page_size = if raw == 1 { 65536 } else { raw };
    if page_size < 512 || !page_size.is_power_of_two() {
        return Err(Error::NotSqlite(format!(
            "page size {page_size} is invalid"
        )));
    }
    let reserved = db[20] as usize;
    Ok(Header {
        page_size,
        usable: page_size - reserved,
        encoding: be32(db, 56).unwrap_or(1).max(1),
    })
}

// ---------------------------------------------------------------------------
// WAL
// ---------------------------------------------------------------------------

struct Wal<'a> {
    /// (page number, frame index, data, file offset), in file order.
    committed: Vec<(u32, u32, &'a [u8], u64)>,
    uncommitted: Vec<(u32, u32, &'a [u8], u64)>,
    db_pages_after_commit: Option<u32>,
}

/// SQLite's WAL checksum (wal.c `walChecksumBytes`): over 8-byte steps, two
/// 32-bit words at a time, in the byte order the magic number names.
fn wal_checksum(big_endian: bool, data: &[u8], mut s1: u32, mut s2: u32) -> (u32, u32) {
    let word = |c: &[u8]| {
        let b = [c[0], c[1], c[2], c[3]];
        if big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        }
    };
    for c in data.chunks_exact(8) {
        s1 = s1.wrapping_add(word(&c[..4])).wrapping_add(s2);
        s2 = s2.wrapping_add(word(&c[4..])).wrapping_add(s1);
    }
    (s1, s2)
}

fn parse_wal(wal: &[u8], page_size: usize) -> Option<Wal<'_>> {
    let magic = be32(wal, 0)?;
    let big_endian = match magic {
        0x377f0682 => false,
        0x377f0683 => true,
        _ => return None,
    };
    if be32(wal, 8)? as usize != page_size {
        return None;
    }
    let (salt1, salt2) = (be32(wal, 16)?, be32(wal, 20)?);
    let (c1, c2) = wal_checksum(big_endian, &wal[..24], 0, 0);
    if (c1, c2) != (be32(wal, 24)?, be32(wal, 28)?) {
        return None;
    }
    let frame = 24 + page_size;
    let mut sums = (c1, c2);
    let mut pending = Vec::new();
    let mut committed = Vec::new();
    let mut db_pages = None;
    let mut i = 0u32;
    let mut at = 32usize;
    while at + frame <= wal.len() {
        let h = &wal[at..at + 24];
        let data = &wal[at + 24..at + frame];
        if be32(h, 8)? != salt1 || be32(h, 12)? != salt2 {
            break;
        }
        let s = wal_checksum(big_endian, &h[..8], sums.0, sums.1);
        let s = wal_checksum(big_endian, data, s.0, s.1);
        if s != (be32(h, 16)?, be32(h, 20)?) {
            break;
        }
        sums = s;
        pending.push((be32(h, 0)?, i, data, (at + 24) as u64));
        let commit_size = be32(h, 4)?;
        if commit_size != 0 {
            committed.append(&mut pending);
            db_pages = Some(commit_size);
        }
        i += 1;
        at += frame;
    }
    Some(Wal {
        committed,
        uncommitted: pending,
        db_pages_after_commit: db_pages,
    })
}

// ---------------------------------------------------------------------------
// the current database, and its schema
// ---------------------------------------------------------------------------

struct Current<'a> {
    pages: BTreeMap<u32, PageRef<'a>>,
    /// Page versions replaced by a later one.
    older: Vec<PageRef<'a>>,
    uncommitted: Vec<PageRef<'a>>,
}

fn current<'a>(db: &'a [u8], wal: Option<&Wal<'a>>, page_size: usize) -> Current<'a> {
    let mut pages = BTreeMap::new();
    let n = (db.len() / page_size) as u32;
    for p in 1..=n {
        let off = (p as usize - 1) * page_size;
        pages.insert(
            p,
            PageRef {
                number: p,
                data: &db[off..off + page_size],
                file: "db",
                file_offset: off as u64,
            },
        );
    }
    let mut older = Vec::new();
    let mut uncommitted = Vec::new();
    if let Some(w) = wal {
        for &(pg, _frame, data, off) in &w.committed {
            let new = PageRef {
                number: pg,
                data,
                file: "wal",
                file_offset: off,
            };
            if let Some(old) = pages.insert(pg, new) {
                older.push(old);
            }
        }
        if let Some(size) = w.db_pages_after_commit {
            let beyond: Vec<u32> = pages.range(size + 1..).map(|(k, _)| *k).collect();
            for k in beyond {
                if let Some(old) = pages.remove(&k) {
                    older.push(old);
                }
            }
        }
        for &(pg, _frame, data, off) in &w.uncommitted {
            uncommitted.push(PageRef {
                number: pg,
                data,
                file: "wal",
                file_offset: off,
            });
        }
    }
    Current {
        pages,
        older,
        uncommitted,
    }
}

fn wal_frame_of(r: &PageRef, page_size: usize) -> Option<u32> {
    (r.file == "wal").then(|| ((r.file_offset - 32 - 24) / (24 + page_size as u64)) as u32)
}

/// Offset of the b-tree page header within a page.
fn header_at(page: u32) -> usize {
    if page == 1 {
        100
    } else {
        0
    }
}

/// Cells of a table b-tree page: leaf cells as (rowid, record offset, payload),
/// interior cells as child page numbers, plus the right-most child.
enum Parsed {
    Leaf(Vec<(i64, usize, Vec<u8>)>),
    Interior(Vec<u32>),
    Other,
}

fn parse_table_page(
    r: &PageRef,
    usable: usize,
    pages: &BTreeMap<u32, PageRef>,
    overflow_seen: &mut HashSet<u32>,
) -> Parsed {
    let d = r.data;
    let h = header_at(r.number);
    let kind = d[h];
    let Some(cells) = be16(d, h + 3) else {
        return Parsed::Other;
    };
    match kind {
        0x05 => {
            let mut kids = Vec::new();
            for i in 0..cells {
                let Some(ptr) = be16(d, h + 12 + 2 * i) else {
                    return Parsed::Other;
                };
                match be32(d, ptr) {
                    Some(c) => kids.push(c),
                    None => return Parsed::Other,
                }
            }
            if let Some(right) = be32(d, h + 8) {
                kids.push(right);
            }
            Parsed::Interior(kids)
        }
        0x0D => {
            let mut out = Vec::new();
            if h + 8 + 2 * cells > d.len() {
                return Parsed::Other;
            }
            for i in 0..cells {
                let Some(ptr) = be16(d, h + 8 + 2 * i) else {
                    continue;
                };
                let Some((plen, a)) = varint(d, ptr) else {
                    continue;
                };
                let Some((rowid, b)) = varint(d, ptr + a) else {
                    continue;
                };
                let plen = plen as usize;
                let start = ptr + a + b;
                let local = local_payload(plen, usable);
                let Some(head) = d.get(start..start + local) else {
                    continue;
                };
                let mut payload = head.to_vec();
                if local < plen {
                    let Some(mut next) = be32(d, start + local) else {
                        continue;
                    };
                    while payload.len() < plen && next != 0 && overflow_seen.insert(next) {
                        let Some(o) = pages.get(&next) else { break };
                        let take = (plen - payload.len()).min(usable - 4);
                        payload.extend_from_slice(&o.data[4..4 + take]);
                        next = be32(o.data, 0).unwrap_or(0);
                    }
                    if payload.len() < plen {
                        continue;
                    }
                }
                out.push((rowid as i64, start, payload));
            }
            Parsed::Leaf(out)
        }
        _ => Parsed::Other,
    }
}

fn parse_columns(sql: &str) -> Option<Vec<Column>> {
    let open = sql.find('(')?;
    let close = sql.rfind(')')?;
    let inner = sql.get(open + 1..close)?;
    let mut parts = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    let mut quote: Option<char> = None;
    for (i, ch) in inner.char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(ch),
            (None, '[') => quote = Some(']'),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, ',') if depth == 0 => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    let mut cols = Vec::new();
    for p in parts {
        let p = p.trim();
        let upper = p.to_ascii_uppercase();
        if ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"]
            .iter()
            .any(|k| upper.starts_with(k))
        {
            continue;
        }
        let (name, rest) = split_name(p);
        let decl: String = rest
            .split_whitespace()
            .take_while(|w| {
                !matches!(
                    w.to_ascii_uppercase().as_str(),
                    "PRIMARY"
                        | "NOT"
                        | "NULL"
                        | "DEFAULT"
                        | "UNIQUE"
                        | "CHECK"
                        | "REFERENCES"
                        | "COLLATE"
                        | "CONSTRAINT"
                        | "GENERATED"
                        | "AS"
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let rowid_alias = decl.eq_ignore_ascii_case("INTEGER") && upper.contains("PRIMARY KEY");
        cols.push(Column {
            name,
            decl_type: decl,
            rowid_alias,
        });
    }
    Some(cols)
}

fn split_name(p: &str) -> (String, &str) {
    let first = p.chars().next().unwrap_or(' ');
    let close = match first {
        '"' => Some('"'),
        '`' => Some('`'),
        '[' => Some(']'),
        '\'' => Some('\''),
        _ => None,
    };
    match close {
        Some(c) => match p[1..].find(c) {
            Some(end) => (p[1..1 + end].to_string(), &p[end + 2..]),
            None => (p.to_string(), ""),
        },
        None => match p.find(char::is_whitespace) {
            Some(i) => (p[..i].to_string(), &p[i..]),
            None => (p.to_string(), ""),
        },
    }
}

/// Type affinity from a declared type (sqlite.org/datatype3.html 3.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

fn affinity(decl: &str) -> Affinity {
    let d = decl.to_ascii_uppercase();
    if d.contains("INT") {
        Affinity::Integer
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        Affinity::Text
    } else if d.contains("BLOB") || d.is_empty() {
        Affinity::Blob
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// Can a column of this affinity hold a value of this serial type on disk?
fn fits(col: &Column, t: u64) -> bool {
    if col.rowid_alias {
        return t == 0;
    }
    let int = matches!(t, 0..=6 | 8 | 9);
    let text = t >= 13 && t % 2 == 1;
    let blob = t >= 12 && t % 2 == 0;
    match affinity(&col.decl_type) {
        // Text that looks like an integer is converted on insert; other text
        // stays text.
        Affinity::Integer | Affinity::Numeric => int || t == 7 || text || blob,
        Affinity::Real => int || t == 7 || text || blob,
        // A TEXT column can hold a blob, but almost nothing stores one there,
        // and allowing it lets a misaligned parse read record bytes as a value.
        Affinity::Text => t == 0 || text,
        Affinity::Blob => !matches!(t, 10 | 11),
    }
}

/// Is `values` a believable row, beyond fitting the types? Text must be
/// printable, at least a third of the columns must hold something, and a row of
/// nothing but NULLs and zeros is not evidence: zeroed space parses as NULLs.
fn believable(values: &[Value]) -> bool {
    let non_null = values.iter().filter(|v| !matches!(v, Value::Null)).count();
    if non_null * 3 < values.len() {
        return false;
    }
    let mut informative = 0;
    for v in values {
        match v {
            Value::Text(t) => {
                if t.chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
                {
                    return false;
                }
                if !t.is_empty() {
                    informative += 1;
                }
            }
            Value::Integer(i) if *i != 0 => informative += 1,
            Value::Real(_) | Value::Blob(_) => informative += 1,
            _ => {}
        }
    }
    informative >= 2
}

// ---------------------------------------------------------------------------
// carving
// ---------------------------------------------------------------------------

/// Parse a record whose header starts at `s`, for a table of `table.columns`.
/// Returns the values and the record's total length.
fn record_at(d: &[u8], s: usize, table: &Table, encoding: u32) -> Option<(Vec<Value>, usize)> {
    let n = table.columns.len();
    let (h, hl) = varint(d, s)?;
    let h = h as usize;
    if h < hl + n || h > hl + 9 * n {
        return None;
    }
    let mut at = s + hl;
    let mut types = Vec::with_capacity(n);
    while at < s + h {
        if types.len() == n {
            return None;
        }
        let (t, l) = varint(d, at)?;
        if !fits(&table.columns[types.len()], t) {
            return None;
        }
        types.push(t);
        at += l;
    }
    if at != s + h || types.len() != n {
        return None;
    }
    let body: usize = types
        .iter()
        .map(|&t| serial_len(t))
        .sum::<Option<usize>>()?;
    let total = h + body;
    let payload = d.get(s..s + total)?;
    let values = decode_record(payload, encoding)?;
    believable(&values).then_some((values, total))
}

/// Parse a record whose header-size byte was overwritten, from its serial types
/// at `types_at`. A freeblock's four-byte header lands on a cell's payload
/// length, rowid and header size when those take four bytes or fewer - a rowid
/// below 128 and a payload under 16384 bytes - so the serial types start right
/// after it. With a one-byte payload length the first serial type is lost too;
/// when that column is the rowid alias it can only have been NULL, so it is
/// restored (`alias_lost`).
fn record_headerless(
    d: &[u8],
    types_at: usize,
    table: &Table,
    encoding: u32,
    alias_lost: bool,
) -> Option<(Vec<Value>, usize)> {
    let n = table.columns.len();
    if alias_lost && !table.columns.first()?.rowid_alias {
        return None;
    }
    let mut types = Vec::with_capacity(n);
    let mut raw = Vec::new();
    if alias_lost {
        types.push(0u64);
        raw.push(0u8);
    }
    let mut at = types_at;
    while types.len() < n {
        let (t, l) = varint(d, at)?;
        if !fits(&table.columns[types.len()], t) {
            return None;
        }
        types.push(t);
        raw.extend_from_slice(d.get(at..at + l)?);
        at += l;
    }
    let body: usize = types
        .iter()
        .map(|&t| serial_len(t))
        .sum::<Option<usize>>()?;
    if raw.len() + 1 >= 128 {
        return None; // a header this long would have kept its size byte
    }
    let mut payload = vec![(raw.len() + 1) as u8];
    payload.extend_from_slice(&raw);
    payload.extend_from_slice(d.get(at..at + body)?);
    let values = decode_record(&payload, encoding)?;
    believable(&values).then_some((values, at + body - types_at))
}

/// If a cell header ending at `s` survives, the rowid it names.
fn rowid_before(d: &[u8], s: usize, record_len: usize) -> Option<i64> {
    for lr in 1..=9usize {
        for lp in 1..=3usize {
            let Some(start) = s.checked_sub(lr + lp) else {
                continue;
            };
            let Some((plen, a)) = varint(d, start) else {
                continue;
            };
            if a != lp || plen as usize != record_len {
                continue;
            }
            if let Some((rowid, b)) = varint(d, start + lp) {
                if b == lr {
                    return Some(rowid as i64);
                }
            }
        }
    }
    None
}

struct Carver<'a> {
    tables: &'a [Table],
    encoding: u32,
    found: Vec<RecoveredRow>,
}

impl Carver<'_> {
    /// Search `d[start..end]` for records of any table. Records may not run
    /// past `limit` (the end of the page).
    fn scan(&mut self, r: &PageRef, start: usize, end: usize, source: &Source) {
        let d = r.data;
        let mut s = start;
        while s < end {
            let mut advanced = false;
            for t in self.tables {
                if let Some((values, len)) = record_at(d, s, t, self.encoding) {
                    self.found.push(RecoveredRow {
                        table: t.name.clone(),
                        rowid: rowid_before(d, s, len),
                        values,
                        source: source.clone(),
                        file: r.file,
                        offset: r.file_offset + s as u64,
                    });
                    s += len;
                    advanced = true;
                    break;
                }
            }
            if !advanced {
                s += 1;
            }
        }
    }

    /// The free regions of a live b-tree page: the gap after the cell pointer
    /// array, and the freeblock chain.
    fn free_space(&mut self, r: &PageRef) {
        let d = r.data;
        let h = header_at(r.number);
        let leaf = d[h] == 0x0D || d[h] == 0x0A;
        let hdr = if leaf { 8 } else { 12 };
        let cells = be16(d, h + 3).unwrap_or(0);
        let content = match be16(d, h + 5).unwrap_or(0) {
            0 => 65536,
            c => c,
        }
        .min(d.len());
        let gap = h + hdr + 2 * cells;
        if gap < content {
            self.scan(
                r,
                gap,
                content,
                &Source::UnallocatedSpace { page: r.number },
            );
        }
        let mut fb = be16(d, h + 1).unwrap_or(0);
        let mut guard = 0;
        while fb != 0 && fb + 4 <= d.len() && guard < 10_000 {
            let size = be16(d, fb + 2).unwrap_or(0);
            let end = (fb + size).min(d.len());
            let mut from = fb;
            'first: for alias_lost in [false, true] {
                for t in self.tables {
                    if let Some((values, len)) =
                        record_headerless(d, fb + 4, t, self.encoding, alias_lost)
                    {
                        self.found.push(RecoveredRow {
                            table: t.name.clone(),
                            rowid: None,
                            values,
                            source: Source::Freeblock { page: r.number },
                            file: r.file,
                            offset: r.file_offset + fb as u64 + 4,
                        });
                        from = fb + 4 + len;
                        break 'first;
                    }
                }
            }
            if from < end {
                self.scan(r, from, end, &Source::Freeblock { page: r.number });
            }
            let next = be16(d, fb).unwrap_or(0);
            if next <= fb {
                break;
            }
            fb = next;
            guard += 1;
        }
    }

    /// A whole page that is not part of the current database: its cells if it
    /// still parses as a table leaf, then every byte.
    fn dead_page(&mut self, r: &PageRef, source: Source, skip: usize) {
        let d = r.data;
        let h = header_at(r.number);
        if d[h] == 0x0D && skip == 0 {
            if let Some(cells) = be16(d, h + 3) {
                if h + 8 + 2 * cells <= d.len() {
                    for i in 0..cells {
                        let Some(ptr) = be16(d, h + 8 + 2 * i) else {
                            continue;
                        };
                        let Some((plen, a)) = varint(d, ptr) else {
                            continue;
                        };
                        let Some((rowid, b)) = varint(d, ptr + a) else {
                            continue;
                        };
                        let s = ptr + a + b;
                        for t in self.tables {
                            if let Some((values, len)) = record_at(d, s, t, self.encoding) {
                                if len == plen as usize {
                                    self.found.push(RecoveredRow {
                                        table: t.name.clone(),
                                        rowid: Some(rowid as i64),
                                        values,
                                        source: source.clone(),
                                        file: r.file,
                                        offset: r.file_offset + s as u64,
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        // Everything else on the page, including cells the pointers no longer
        // reach. Duplicates of the cells above are removed later.
        self.scan(r, skip, d.len(), &source);
    }
}

/// Carve a database, with its `-wal` if there is one.
pub fn carve(db: &[u8], wal: Option<&[u8]>) -> Result<Report> {
    let hdr = parse_header(db)?;
    let mut notes = Vec::new();
    let wal = match wal {
        Some(w) => match parse_wal(w, hdr.page_size) {
            Some(parsed) => Some(parsed),
            None => {
                notes.push(
                    "the WAL file has no valid header for this database's page size; ignored"
                        .into(),
                );
                None
            }
        },
        None => None,
    };
    let cur = current(db, wal.as_ref(), hdr.page_size);
    let page1 = cur
        .pages
        .get(&1)
        .ok_or_else(|| Error::NotSqlite("no page 1".into()))?;
    if &page1.data[..16] != HEADER_MAGIC {
        return Err(Error::NotSqlite("page 1 has no header".into()));
    }

    // --- walk the current database ------------------------------------------
    let mut overflow_seen = HashSet::new();
    let mut reached: HashMap<u32, Option<usize>> = HashMap::new(); // page -> table index
    let walk = |root: u32,
                table: Option<usize>,
                reached: &mut HashMap<u32, Option<usize>>,
                overflow_seen: &mut HashSet<u32>|
     -> Vec<(i64, Vec<u8>)> {
        let mut rows = Vec::new();
        let mut stack = vec![root];
        while let Some(p) = stack.pop() {
            if reached.contains_key(&p) {
                continue;
            }
            let Some(r) = cur.pages.get(&p) else { continue };
            reached.insert(p, table);
            match parse_table_page(r, hdr.usable, &cur.pages, overflow_seen) {
                Parsed::Leaf(cells) => rows.extend(cells.into_iter().map(|(id, _, pl)| (id, pl))),
                Parsed::Interior(kids) => stack.extend(kids.into_iter().rev()),
                Parsed::Other => {}
            }
        }
        rows
    };

    let mut tables = Vec::new();
    let mut other_roots = Vec::new();
    for (_, payload) in walk(1, None, &mut reached, &mut overflow_seen) {
        let Some(v) = decode_record(&payload, hdr.encoding) else {
            continue;
        };
        let text = |i: usize| match v.get(i) {
            Some(Value::Text(t)) => Some(t.clone()),
            _ => None,
        };
        let root = match v.get(3) {
            Some(Value::Integer(r)) => *r as u32,
            _ => 0,
        };
        let (Some(kind), Some(name), Some(sql)) = (text(0), text(1), text(4)) else {
            if root != 0 {
                other_roots.push(root);
            }
            continue;
        };
        if kind == "table" && !sql.to_ascii_uppercase().contains("WITHOUT ROWID") {
            if let Some(columns) = parse_columns(&sql) {
                tables.push(Table {
                    name,
                    root_page: root,
                    columns,
                    live_rows: 0,
                });
                continue;
            }
        }
        if kind == "table" {
            notes.push(format!(
                "table {name} is WITHOUT ROWID or unparsed; not carved"
            ));
        }
        if root != 0 {
            other_roots.push(root);
        }
    }

    let mut live_keys: HashSet<(usize, String)> = HashSet::new();
    for (i, t) in tables.iter_mut().enumerate() {
        let rows = walk(t.root_page, Some(i), &mut reached, &mut overflow_seen);
        t.live_rows = rows.len();
        for (_, payload) in rows {
            if let Some(v) = decode_record(&payload, hdr.encoding) {
                live_keys.insert((i, row_key(&v)));
            }
        }
    }
    // Index b-trees are reached but not carved.
    for root in other_roots {
        let mut stack = vec![root];
        while let Some(p) = stack.pop() {
            if reached.contains_key(&p) {
                continue;
            }
            let Some(r) = cur.pages.get(&p) else { continue };
            reached.insert(p, None);
            let d = r.data;
            let h = header_at(p);
            if d[h] == 0x02 || d[h] == 0x05 {
                let cells = be16(d, h + 3).unwrap_or(0);
                for i in 0..cells {
                    if let Some(c) = be16(d, h + 12 + 2 * i).and_then(|ptr| be32(d, ptr)) {
                        stack.push(c);
                    }
                }
                if let Some(right) = be32(d, h + 8) {
                    stack.push(right);
                }
            }
        }
    }

    // Freelist.
    let mut freelist_trunk = HashMap::new();
    let mut freelist_leaf = HashSet::new();
    let mut trunk = be32(page1.data, 32).unwrap_or(0);
    while trunk != 0 && !freelist_trunk.contains_key(&trunk) {
        let Some(r) = cur.pages.get(&trunk) else {
            break;
        };
        let n = be32(r.data, 4).unwrap_or(0) as usize;
        let n = n.min((hdr.usable - 8) / 4);
        freelist_trunk.insert(trunk, 8 + 4 * n);
        for i in 0..n {
            if let Some(leaf) = be32(r.data, 8 + 4 * i) {
                freelist_leaf.insert(leaf);
            }
        }
        trunk = be32(r.data, 0).unwrap_or(0);
    }

    // --- carve ---------------------------------------------------------------
    let mut carver = Carver {
        tables: &tables,
        encoding: hdr.encoding,
        found: Vec::new(),
    };
    for (&p, r) in &cur.pages {
        if let Some(&skip) = freelist_trunk.get(&p) {
            carver.dead_page(r, Source::FreelistPage { page: p }, skip);
        } else if freelist_leaf.contains(&p) {
            carver.dead_page(r, Source::FreelistPage { page: p }, 0);
        } else if reached.contains_key(&p) {
            if !overflow_seen.contains(&p) {
                carver.free_space(r);
            }
        } else if !overflow_seen.contains(&p) {
            carver.dead_page(r, Source::UnreferencedPage { page: p }, 0);
        }
    }
    for r in &cur.older {
        let src = Source::OlderPageVersion {
            page: r.number,
            wal_frame: wal_frame_of(r, hdr.page_size),
        };
        carver.dead_page(r, src, 0);
    }
    for r in &cur.uncommitted {
        let src = Source::UncommittedWalFrame {
            page: r.number,
            wal_frame: wal_frame_of(r, hdr.page_size).unwrap_or(0),
        };
        carver.dead_page(r, src, 0);
    }
    let found = std::mem::take(&mut carver.found);

    // --- drop live copies and duplicates -------------------------------------
    let index: HashMap<&str, usize> = tables
        .iter()
        .enumerate()
        .map(|(i, t)| (t.name.as_str(), i))
        .collect();
    let mut seen: HashMap<(usize, String), usize> = HashMap::new();
    let mut recovered: Vec<RecoveredRow> = Vec::new();
    for row in found {
        let ti = index[row.table.as_str()];
        let key = (ti, row_key(&row.values));
        if live_keys.contains(&key) {
            continue;
        }
        match seen.get(&key) {
            Some(&at) => {
                if recovered[at].rowid.is_none() && row.rowid.is_some() {
                    recovered[at] = row;
                }
            }
            None => {
                seen.insert(key, recovered.len());
                recovered.push(row);
            }
        }
    }
    // With a rowid, the alias column's value is known.
    for row in &mut recovered {
        let t = &tables[index[row.table.as_str()]];
        if let Some(id) = row.rowid {
            for (c, v) in t.columns.iter().zip(row.values.iter_mut()) {
                if c.rowid_alias {
                    *v = Value::Integer(id);
                }
            }
        }
    }

    Ok(Report {
        page_size: hdr.page_size,
        pages: cur.pages.len() as u32,
        wal_frames_committed: wal.as_ref().map_or(0, |w| w.committed.len() as u32),
        wal_frames_uncommitted: wal.as_ref().map_or(0, |w| w.uncommitted.len() as u32),
        tables,
        recovered,
        notes,
    })
}

/// A row's identity for comparison: every value, with rowid alias columns
/// (NULL on disk) included as they are stored.
fn row_key(values: &[Value]) -> String {
    values
        .iter()
        .map(Value::key)
        .collect::<Vec<_>>()
        .join("\u{1}")
}

/// The live rows of one table, read from bytes (main file plus committed WAL
/// frames), with rowid alias columns filled in. For reading evidence databases,
/// such as an iOS backup's `Manifest.db` and `Photos.sqlite`, without SQLite
/// creating `-shm`/`-wal` files beside them or checkpointing anything.
pub fn read_table(
    db: &[u8],
    wal: Option<&[u8]>,
    table: &str,
) -> Result<(Vec<Column>, Vec<Vec<Value>>)> {
    let hdr = parse_header(db)?;
    let wal = wal.and_then(|w| parse_wal(w, hdr.page_size));
    let cur = current(db, wal.as_ref(), hdr.page_size);
    let mut overflow_seen = HashSet::new();
    let leaf_rows = |root: u32, overflow_seen: &mut HashSet<u32>| {
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        while let Some(p) = stack.pop() {
            if !seen.insert(p) {
                continue;
            }
            let Some(r) = cur.pages.get(&p) else { continue };
            match parse_table_page(r, hdr.usable, &cur.pages, overflow_seen) {
                Parsed::Leaf(cells) => rows.extend(cells.into_iter().map(|(id, _, pl)| (id, pl))),
                Parsed::Interior(kids) => stack.extend(kids.into_iter().rev()),
                Parsed::Other => {}
            }
        }
        rows
    };
    for (_, payload) in leaf_rows(1, &mut overflow_seen) {
        let Some(v) = decode_record(&payload, hdr.encoding) else {
            continue;
        };
        let is = |i: usize, s: &str| matches!(v.get(i), Some(Value::Text(t)) if t.eq_ignore_ascii_case(s));
        if !(is(0, "table") && is(1, table)) {
            continue;
        }
        let (Some(Value::Integer(root)), Some(Value::Text(sql))) = (v.get(3), v.get(4)) else {
            continue;
        };
        let columns = parse_columns(sql)
            .ok_or_else(|| Error::NotSqlite(format!("cannot parse the schema of {table}")))?;
        let mut rows = Vec::new();
        for (rowid, payload) in leaf_rows(*root as u32, &mut overflow_seen) {
            let Some(mut values) = decode_record(&payload, hdr.encoding) else {
                continue;
            };
            // Columns added by ALTER TABLE are absent from older rows.
            values.resize(columns.len(), Value::Null);
            for (c, v) in columns.iter().zip(values.iter_mut()) {
                if c.rowid_alias {
                    *v = Value::Integer(rowid);
                }
            }
            rows.push(values);
        }
        return Ok((columns, rows));
    }
    Err(Error::NoSuchTable(table.to_string()))
}

/// Read a database and its `-wal` next to it, if present. Read-only: nothing is
/// created or modified, unlike opening the database with SQLite.
pub fn carve_file(path: &std::path::Path) -> Result<Report> {
    let db = std::fs::read(path)?;
    let mut wal_path = path.as_os_str().to_owned();
    wal_path.push("-wal");
    let wal = std::fs::read(std::path::PathBuf::from(wal_path)).ok();
    carve(&db, wal.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_from_create_table() {
        let cols = parse_columns(
            "CREATE TABLE sms (_id INTEGER PRIMARY KEY, \"thread id\" INTEGER, address TEXT, \
             body TEXT DEFAULT 'a,b', n NUMERIC(10,2), UNIQUE(address))",
        )
        .unwrap();
        let names: Vec<_> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["_id", "thread id", "address", "body", "n"]);
        assert!(cols[0].rowid_alias && !cols[1].rowid_alias);
        assert_eq!(cols[4].decl_type, "NUMERIC(10,2)");
    }

    #[test]
    fn wal_checksum_matches_a_header_sqlite_wrote() {
        // The first 24 bytes and stored checksum of a WAL written by SQLite
        // 3.50.4 (Python's sqlite3) for testdata/sqlite/make_sms_db.py.
        let mut h = Vec::new();
        for w in [0x377f0682u32, 3007000, 4096, 1, 0x7035aa53, 0x5ceba28c] {
            h.extend_from_slice(&w.to_be_bytes());
        }
        assert_eq!(wal_checksum(false, &h, 0, 0), (0x29913883, 0x6f1bfb84));
        for w in [0x29913883u32, 0x6f1bfb84] {
            h.extend_from_slice(&w.to_be_bytes());
        }
        assert!(parse_wal(&h, 4096).is_some());
        h[9] ^= 1;
        assert!(parse_wal(&h, 4096).is_none());
    }
}
