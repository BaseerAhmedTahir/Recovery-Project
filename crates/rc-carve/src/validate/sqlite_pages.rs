//! Walk a SQLite database's page structure, and say where it breaks.
//!
//! The header tells you how big a database is and nothing about whether its
//! pages are its own. A page overwritten with anything at all - another file's
//! cluster, random bytes, a pattern - leaves the header intact, so a header-only
//! validator calls the file Valid. Measured on the overwritten fixture: a
//! six-page database with one page of random bytes rated GREEN.
//!
//! So the database is walked the way SQLite itself reads it, per the file format
//! document (sqlite.org/fileformat2.html):
//!
//! - page 1 holds the schema table; its rows name the root page of every other
//!   table and index;
//! - every b-tree is walked from its root: each page's type byte, cell count,
//!   cell-content start, freeblock chain and cell pointers must be consistent
//!   with the page, and every child pointer must name a page in the file;
//! - every overflow chain a cell points to is followed;
//! - the freelist trunk chain and its leaf pointers are followed;
//! - no page may be reached twice.
//!
//! The first page that fails is reported, which is what scoring needs to place
//! the damage. Pages the walk never reaches are not judged: a healthy database
//! accounts for all of them, but nothing here can say what an orphan should
//! have held, so its content cannot be called wrong.
//!
//! Bounded: each page is visited at most once, so the walk is linear in the file.

use super::be32;

/// Where and why the page structure fails.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Damage {
    /// 1-based page number.
    pub page: u32,
    pub why: String,
}

/// What a clean walk established.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Walked {
    pub btree_pages: u32,
    pub overflow_pages: u32,
    pub freelist_pages: u32,
    pub trees: u32,
}

fn be16(d: &[u8], at: usize) -> Option<usize> {
    Some(u16::from_be_bytes([*d.get(at)?, *d.get(at + 1)?]) as usize)
}

/// A SQLite varint: one to nine bytes, big-endian seven-bit groups, the ninth
/// byte contributing all eight bits. Returns the value and its length.
fn varint(d: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..9 {
        let b = *d.get(at + i)?;
        if i == 8 {
            return Some(((v << 8) | b as u64, 9));
        }
        v = (v << 7) | (b & 0x7F) as u64;
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

struct Walk<'a> {
    d: &'a [u8],
    page_size: usize,
    usable: usize,
    page_count: u32,
    visited: Vec<bool>,
    out: Walked,
}

impl<'a> Walk<'a> {
    fn page(&self, n: u32) -> &'a [u8] {
        let start = (n as usize - 1) * self.page_size;
        &self.d[start..start + self.page_size]
    }

    fn damage(page: u32, why: impl Into<String>) -> Result<(), Damage> {
        Err(Damage {
            page,
            why: why.into(),
        })
    }

    /// Claim a page for one purpose. Reaching a page twice means two structures
    /// point at it, which a consistent database never does.
    fn claim(&mut self, from: u32, n: u32, what: &str) -> Result<(), Damage> {
        if n == 0 || n > self.page_count {
            return Self::damage(
                from,
                format!(
                    "a {what} pointer names page {n} in a {}-page database",
                    self.page_count
                ),
            );
        }
        if self.visited[n as usize] {
            return Self::damage(
                from,
                format!("page {n} is reached twice, the second time as a {what}"),
            );
        }
        self.visited[n as usize] = true;
        Ok(())
    }

    /// Bytes of a cell's payload stored on the page itself (fileformat2 2.3.1.2).
    fn local_payload(&self, payload: u64, table_leaf: bool) -> u64 {
        let u = self.usable as u64;
        let x = if table_leaf {
            u - 35
        } else {
            ((u - 12) * 64 / 255) - 23
        };
        if payload <= x {
            return payload;
        }
        let m = ((u - 12) * 32 / 255) - 23;
        let k = m + ((payload - m) % (u - 4));
        if k <= x {
            k
        } else {
            m
        }
    }

    /// Follow one overflow chain from its first page.
    fn overflow(&mut self, from: u32, first: u32, mut remaining: u64) -> Result<(), Damage> {
        let mut prev = from;
        let mut n = first;
        let per = (self.usable - 4) as u64;
        while remaining > 0 {
            self.claim(prev, n, "overflow page")?;
            self.out.overflow_pages += 1;
            let next = be32(self.page(n), 0).unwrap_or(0);
            remaining = remaining.saturating_sub(per);
            if remaining == 0 && next != 0 {
                return Self::damage(n, "an overflow chain continues past the end of its payload");
            }
            if remaining > 0 && next == 0 {
                return Self::damage(n, "an overflow chain ends before its payload does");
            }
            prev = n;
            n = next;
        }
        Ok(())
    }

    /// Walk one b-tree. When `schema` is set, collect the root page of every
    /// table and index its leaf rows name.
    fn btree(&mut self, root: u32, from: u32, schema: Option<&mut Vec<u32>>) -> Result<(), Damage> {
        let mut roots_out = schema;
        let mut stack = vec![(root, from)];
        let mut first = true;
        while let Some((n, parent)) = stack.pop() {
            if first {
                // The root was claimed by whoever named it (or is page 1).
                first = false;
            } else {
                self.claim(parent, n, "child page")?;
            }
            let p = self.page(n);
            let h = if n == 1 { 100 } else { 0 };
            let kind = p[h];
            let (interior, table) = match kind {
                0x02 => (true, false),
                0x05 => (true, true),
                0x0A => (false, false),
                0x0D => (false, true),
                other => {
                    return Self::damage(
                        n,
                        format!("page {n} should be a b-tree page but has type byte 0x{other:02X}"),
                    )
                }
            };
            self.out.btree_pages += 1;
            let hdr = if interior { 12 } else { 8 };
            let cells = be16(p, h + 3).unwrap_or(0);
            let content = match be16(p, h + 5).unwrap_or(0) {
                0 => 65536,
                c => c,
            };
            let array_end = h + hdr + 2 * cells;
            if array_end > self.usable {
                return Self::damage(
                    n,
                    format!("page {n} claims {cells} cells, more than the page can index"),
                );
            }
            // Cells live between the end of the pointer array and the end of the
            // usable page; an empty page starts its content area at that end.
            if content < array_end || content > self.usable {
                return Self::damage(
                    n,
                    format!("page {n}'s cell content starts at {content}, outside the page"),
                );
            }
            if p[h + 7] > 60 {
                return Self::damage(
                    n,
                    format!(
                        "page {n} claims {} fragmented bytes; SQLite never leaves over 60",
                        p[h + 7]
                    ),
                );
            }
            // Freeblocks: a chain of (next, size) in ascending order within the
            // content area.
            let mut fb = be16(p, h + 1).unwrap_or(0);
            let mut hops = 0;
            while fb != 0 {
                hops += 1;
                if fb < array_end || fb + 4 > self.usable || hops > self.page_size / 4 {
                    return Self::damage(n, format!("page {n}'s freeblock chain leaves the page"));
                }
                let next = be16(p, fb).unwrap_or(0);
                if next != 0 && next <= fb {
                    return Self::damage(n, format!("page {n}'s freeblock chain does not ascend"));
                }
                fb = next;
            }

            for i in 0..cells {
                let off = be16(p, h + hdr + 2 * i).unwrap_or(0);
                if off < array_end || off < content || off >= self.usable {
                    return Self::damage(
                        n,
                        format!(
                            "page {n} cell {i} points at offset {off}, outside its content area"
                        ),
                    );
                }
                let mut at = off;
                if interior {
                    let child = be32(p, at).unwrap_or(0);
                    stack.push((child, n));
                    at += 4;
                    if table {
                        continue; // an interior table cell is a child and a rowid
                    }
                }
                let Some((payload, len)) = varint(p, at) else {
                    return Self::damage(n, format!("page {n} cell {i} runs off the page"));
                };
                at += len;
                if table {
                    // leaf table: rowid follows the payload size
                    let Some((_, len)) = varint(p, at) else {
                        return Self::damage(n, format!("page {n} cell {i} runs off the page"));
                    };
                    at += len;
                }
                let local = self.local_payload(payload, table && !interior) as usize;
                if at + local > self.usable {
                    return Self::damage(
                        n,
                        format!("page {n} cell {i}'s payload runs past the end of the page"),
                    );
                }
                if let Some(roots) = roots_out.as_deref_mut() {
                    if table && !interior {
                        if let Some(r) = schema_rootpage(&p[at..at + local]) {
                            if r != 0 {
                                roots.push(r);
                            }
                        }
                    }
                }
                if (local as u64) < payload {
                    let Some(first_overflow) = be32(p, at + local) else {
                        return Self::damage(n, format!("page {n} cell {i} runs off the page"));
                    };
                    self.overflow(n, first_overflow, payload - local as u64)?;
                }
            }
            if interior {
                let right = be32(p, h + 8).unwrap_or(0);
                stack.push((right, n));
            }
        }
        Ok(())
    }

    fn freelist(&mut self, trunk: u32, declared: u32) -> Result<(), Damage> {
        let mut prev = 1;
        let mut n = trunk;
        let mut counted = 0u32;
        let max_leaves = (self.usable / 4).saturating_sub(2) as u32;
        while n != 0 {
            self.claim(prev, n, "freelist trunk")?;
            counted += 1;
            let p = self.page(n);
            let next = be32(p, 0).unwrap_or(0);
            let leaves = be32(p, 4).unwrap_or(0);
            if leaves > max_leaves {
                return Self::damage(
                    n,
                    format!(
                        "freelist trunk page {n} claims {leaves} leaves; a page holds {max_leaves}"
                    ),
                );
            }
            for k in 0..leaves {
                let leaf = be32(p, 8 + 4 * k as usize).unwrap_or(0);
                self.claim(n, leaf, "freelist leaf")?;
                counted += 1;
            }
            prev = n;
            n = next;
        }
        self.out.freelist_pages = counted;
        if counted != declared {
            return Self::damage(
                trunk.max(1),
                format!("the freelist holds {counted} pages but the header says {declared}"),
            );
        }
        Ok(())
    }
}

/// The `rootpage` column of a sqlite_schema row: type, name, tbl_name, rootpage,
/// sql. Returns `None` when the record is not shaped like one.
fn schema_rootpage(rec: &[u8]) -> Option<u32> {
    let (hdr_len, mut at) = varint(rec, 0)?;
    let hdr_len = hdr_len as usize;
    if hdr_len > rec.len() {
        return None;
    }
    let mut types = Vec::with_capacity(5);
    while at < hdr_len && types.len() < 5 {
        let (t, len) = varint(rec, at)?;
        types.push(t);
        at += len;
    }
    if types.len() < 4 {
        return None;
    }
    let size_of = |t: u64| -> Option<usize> {
        Some(match t {
            0 | 8 | 9 => 0,
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 4,
            5 => 6,
            6 | 7 => 8,
            t if t >= 12 => ((t - 12) / 2) as usize,
            _ => return None,
        })
    };
    let mut body = hdr_len;
    for t in &types[..3] {
        body += size_of(*t)?;
    }
    let t = types[3];
    let n = size_of(t)?;
    let bytes = rec.get(body..body + n)?;
    Some(match t {
        8 => 0,
        9 => 1,
        1..=4 => bytes.iter().fold(0u64, |v, &b| (v << 8) | b as u64) as u32,
        _ => return None,
    })
}

/// Walk `d`, a database of `page_count` pages of `page_size`, all present.
///
/// `ptrmap` is true for auto-vacuum databases, whose pointer-map pages are
/// reachable from no b-tree and are skipped rather than judged.
pub(crate) fn walk(
    d: &[u8],
    page_size: usize,
    reserved: usize,
    page_count: u32,
    freelist_trunk: u32,
    freelist_pages: u32,
    ptrmap: bool,
) -> Result<Walked, Damage> {
    let mut w = Walk {
        d,
        page_size,
        usable: page_size - reserved,
        page_count,
        visited: vec![false; page_count as usize + 1],
        out: Walked::default(),
    };
    // SQLite never uses the page holding the byte at 1 GiB (the lock byte).
    let lock_page = (1u64 << 30) / page_size as u64 + 1;
    if lock_page <= page_count as u64 {
        w.visited[lock_page as usize] = true;
    }
    if ptrmap && page_count >= 2 {
        // Pointer maps: page 2, then one every usable/5 + 1 pages.
        let span = (w.usable / 5 + 1) as u32;
        let mut p = 2u32;
        while p <= page_count {
            w.visited[p as usize] = true;
            p += span;
        }
    }

    w.visited[1] = true;
    let mut roots = Vec::new();
    w.btree(1, 1, Some(&mut roots))?;
    w.out.trees = 1;
    for r in roots {
        w.claim(1, r, "table or index root")?;
        w.btree(r, 1, None)?;
        w.out.trees += 1;
    }
    w.freelist(freelist_trunk, freelist_pages)?;
    Ok(w.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_follow_the_sqlite_encoding() {
        assert_eq!(varint(&[0x05], 0), Some((5, 1)));
        assert_eq!(varint(&[0x81, 0x00], 0), Some((128, 2)));
        assert_eq!(varint(&[0xFF; 9], 0), Some((u64::MAX, 9)));
        assert_eq!(varint(&[0x81], 0), None, "runs off the end");
    }

    #[test]
    fn a_schema_row_yields_its_root_page() {
        // header: 6 bytes; types: text(5)=22, text(1)=14, text(1)=14, int8=1, text(0)=13
        let mut rec = vec![6u8, 22, 14, 14, 1, 13];
        rec.extend_from_slice(b"table");
        rec.push(b't');
        rec.push(b't');
        rec.push(7);
        assert_eq!(schema_rootpage(&rec), Some(7));
    }
}
