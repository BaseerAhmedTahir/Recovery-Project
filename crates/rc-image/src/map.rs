//! ddrescue-compatible map file.
//!
//! The sidecar `.map` file records the status of every byte range on the source
//! so an interrupted clone can resume exactly where it stopped, and so the UI
//! can show which regions are unreadable (SPEC.md section 5.2).
//!
//! The on-disk format is GNU ddrescue's, deliberately, so the file can be
//! inspected or resumed with `ddrescue` itself:
//!
//! ```text
//! # Map file created by rc-image
//! 0x00030000     ?               1
//! 0x00000000  0x00030000  +
//! 0x00030000  0x00001000  -
//! ```
//!
//! The first non-comment line is `current_pos current_status current_pass`;
//! every later line is `pos size status`.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, BufWriter, Write};
use std::path::Path;

/// Status of a byte range, using ddrescue's character codes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum BlockStatus {
    /// `?` - not yet attempted.
    NonTried,
    /// `*` - a large-block read failed; not yet retried at a finer size.
    NonTrimmed,
    /// `/` - trimmed down to a failing edge; not yet scraped sector by sector.
    NonScraped,
    /// `-` - confirmed bad; every retry at sector granularity failed.
    Bad,
    /// `+` - successfully copied.
    Finished,
}

impl BlockStatus {
    pub fn as_char(self) -> char {
        match self {
            BlockStatus::NonTried => '?',
            BlockStatus::NonTrimmed => '*',
            BlockStatus::NonScraped => '/',
            BlockStatus::Bad => '-',
            BlockStatus::Finished => '+',
        }
    }

    pub fn from_char(c: char) -> Option<Self> {
        Some(match c {
            '?' => BlockStatus::NonTried,
            '*' => BlockStatus::NonTrimmed,
            '/' => BlockStatus::NonScraped,
            '-' => BlockStatus::Bad,
            '+' => BlockStatus::Finished,
            _ => return None,
        })
    }

    /// True if this range still has some chance of yielding data.
    pub fn is_recoverable(self) -> bool {
        matches!(
            self,
            BlockStatus::NonTried | BlockStatus::NonTrimmed | BlockStatus::NonScraped
        )
    }
}

impl fmt::Display for BlockStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_char())
    }
}

/// A contiguous byte range with a single status.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Block {
    pub pos: u64,
    pub size: u64,
    pub status: BlockStatus,
}

impl Block {
    pub fn end(&self) -> u64 {
        self.pos + self.size
    }
}

/// The full status map for a device.
///
/// Internally a sorted, gap-free, adjacent-merged partition of `[0, size)`
/// keyed by start offset. Keeping it normalised means resume decisions are a
/// simple scan rather than an interval search.
#[derive(Clone, Debug)]
pub struct BlockMap {
    size: u64,
    blocks: BTreeMap<u64, Block>,
    pub current_pos: u64,
    pub current_status: BlockStatus,
    pub current_pass: u32,
}

impl BlockMap {
    /// A map covering `size` bytes, entirely untried.
    pub fn new(size: u64) -> Self {
        let mut blocks = BTreeMap::new();
        if size > 0 {
            blocks.insert(
                0,
                Block {
                    pos: 0,
                    size,
                    status: BlockStatus::NonTried,
                },
            );
        }
        BlockMap {
            size,
            blocks,
            current_pos: 0,
            current_status: BlockStatus::NonTried,
            current_pass: 1,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.blocks.values()
    }

    /// Total bytes in a given status.
    pub fn bytes_in(&self, status: BlockStatus) -> u64 {
        self.blocks
            .values()
            .filter(|b| b.status == status)
            .map(|b| b.size)
            .sum()
    }

    pub fn finished_bytes(&self) -> u64 {
        self.bytes_in(BlockStatus::Finished)
    }

    pub fn bad_bytes(&self) -> u64 {
        self.bytes_in(BlockStatus::Bad)
    }

    /// Status at a specific offset.
    pub fn status_at(&self, pos: u64) -> Option<BlockStatus> {
        self.blocks
            .range(..=pos)
            .next_back()
            .filter(|(_, b)| pos < b.end())
            .map(|(_, b)| b.status)
    }

    /// Mark `[pos, pos+len)` with `status`, splitting and merging as needed.
    pub fn mark(&mut self, pos: u64, len: u64, status: BlockStatus) {
        if len == 0 || pos >= self.size {
            return;
        }
        let end = (pos + len).min(self.size);

        // Split any block straddling the start of the range.
        if let Some((&start, &b)) = self.blocks.range(..pos).next_back() {
            if b.end() > pos {
                self.blocks.insert(
                    start,
                    Block {
                        pos: start,
                        size: pos - start,
                        status: b.status,
                    },
                );
                self.blocks.insert(
                    pos,
                    Block {
                        pos,
                        size: b.end() - pos,
                        status: b.status,
                    },
                );
            }
        }
        // Split any block straddling the end of the range.
        if let Some((&start, &b)) = self.blocks.range(..end).next_back() {
            if b.end() > end {
                self.blocks.insert(
                    start,
                    Block {
                        pos: start,
                        size: end - start,
                        status: b.status,
                    },
                );
                self.blocks.insert(
                    end,
                    Block {
                        pos: end,
                        size: b.end() - end,
                        status: b.status,
                    },
                );
            }
        }

        // Replace everything strictly inside the range.
        let doomed: Vec<u64> = self.blocks.range(pos..end).map(|(&k, _)| k).collect();
        for k in doomed {
            self.blocks.remove(&k);
        }
        self.blocks.insert(
            pos,
            Block {
                pos,
                size: end - pos,
                status,
            },
        );

        self.merge_adjacent();
    }

    /// Collapse neighbouring blocks that share a status.
    fn merge_adjacent(&mut self) {
        let mut merged: Vec<Block> = Vec::with_capacity(self.blocks.len());
        for b in self.blocks.values() {
            match merged.last_mut() {
                Some(prev) if prev.status == b.status && prev.end() == b.pos => {
                    prev.size += b.size;
                }
                _ => merged.push(*b),
            }
        }
        self.blocks = merged.into_iter().map(|b| (b.pos, b)).collect();
    }

    /// First block at or after `from` whose status matches, for pass planning.
    pub fn next_block_with(&self, from: u64, status: BlockStatus) -> Option<Block> {
        self.blocks
            .values()
            .find(|b| b.status == status && b.end() > from)
            .map(|b| {
                let pos = b.pos.max(from);
                Block {
                    pos,
                    size: b.end() - pos,
                    status: b.status,
                }
            })
    }

    /// Every block matching `status`, in order.
    pub fn blocks_with(&self, status: BlockStatus) -> Vec<Block> {
        self.blocks
            .values()
            .filter(|b| b.status == status)
            .copied()
            .collect()
    }

    // --- persistence ------------------------------------------------------

    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let f = std::fs::File::create(path)?;
        let mut w = BufWriter::new(f);
        writeln!(w, "# Map file created by rc-image")?;
        writeln!(w, "# Command line: rc-cli image")?;
        writeln!(w, "# pos        size        status")?;
        writeln!(
            w,
            "0x{:08X}     {}               {}",
            self.current_pos,
            self.current_status.as_char(),
            self.current_pass
        )?;
        for b in self.blocks.values() {
            writeln!(
                w,
                "0x{:08X}  0x{:08X}  {}",
                b.pos,
                b.size,
                b.status.as_char()
            )?;
        }
        w.flush()
    }

    pub fn read_from(path: &Path) -> std::io::Result<Self> {
        let f = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(f);

        let mut blocks: BTreeMap<u64, Block> = BTreeMap::new();
        let mut header: Option<(u64, BlockStatus, u32)> = None;
        let mut size = 0u64;

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            if header.is_none() {
                // current_pos current_status current_pass
                if fields.len() < 2 {
                    continue;
                }
                let pos = parse_u64(fields[0]).unwrap_or(0);
                let st = fields[1]
                    .chars()
                    .next()
                    .and_then(BlockStatus::from_char)
                    .unwrap_or(BlockStatus::NonTried);
                let pass = fields.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
                header = Some((pos, st, pass));
                continue;
            }
            if fields.len() < 3 {
                continue;
            }
            let (Some(pos), Some(sz)) = (parse_u64(fields[0]), parse_u64(fields[1])) else {
                continue;
            };
            let Some(status) = fields[2].chars().next().and_then(BlockStatus::from_char) else {
                continue;
            };
            size = size.max(pos + sz);
            blocks.insert(
                pos,
                Block {
                    pos,
                    size: sz,
                    status,
                },
            );
        }

        let (current_pos, current_status, current_pass) =
            header.unwrap_or((0, BlockStatus::NonTried, 1));

        let mut m = BlockMap {
            size,
            blocks,
            current_pos,
            current_status,
            current_pass,
        };
        m.merge_adjacent();
        Ok(m)
    }
}

fn parse_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_map_is_entirely_untried() {
        let m = BlockMap::new(1000);
        assert_eq!(m.bytes_in(BlockStatus::NonTried), 1000);
        assert_eq!(m.blocks().count(), 1);
        assert_eq!(m.status_at(0), Some(BlockStatus::NonTried));
        assert_eq!(m.status_at(999), Some(BlockStatus::NonTried));
        assert_eq!(m.status_at(1000), None);
    }

    #[test]
    fn marking_splits_and_totals_stay_consistent() {
        let mut m = BlockMap::new(1000);
        m.mark(100, 200, BlockStatus::Finished);
        assert_eq!(m.bytes_in(BlockStatus::Finished), 200);
        assert_eq!(m.bytes_in(BlockStatus::NonTried), 800);
        assert_eq!(m.status_at(99), Some(BlockStatus::NonTried));
        assert_eq!(m.status_at(100), Some(BlockStatus::Finished));
        assert_eq!(m.status_at(299), Some(BlockStatus::Finished));
        assert_eq!(m.status_at(300), Some(BlockStatus::NonTried));
        // The map must always remain a gap-free partition of [0, size).
        assert_eq!(m.blocks().map(|b| b.size).sum::<u64>(), 1000);
    }

    #[test]
    fn adjacent_blocks_with_equal_status_merge() {
        let mut m = BlockMap::new(1000);
        m.mark(0, 100, BlockStatus::Finished);
        m.mark(100, 100, BlockStatus::Finished);
        assert_eq!(
            m.blocks_with(BlockStatus::Finished).len(),
            1,
            "adjacent finished ranges should merge into one block"
        );
        assert_eq!(m.bytes_in(BlockStatus::Finished), 200);
    }

    #[test]
    fn overlapping_marks_overwrite() {
        let mut m = BlockMap::new(1000);
        m.mark(100, 200, BlockStatus::Finished);
        m.mark(150, 100, BlockStatus::Bad);
        assert_eq!(m.bytes_in(BlockStatus::Bad), 100);
        assert_eq!(m.bytes_in(BlockStatus::Finished), 100);
        assert_eq!(m.blocks().map(|b| b.size).sum::<u64>(), 1000);
    }

    #[test]
    fn marks_clamp_to_device_size() {
        let mut m = BlockMap::new(1000);
        m.mark(900, 500, BlockStatus::Finished);
        assert_eq!(m.bytes_in(BlockStatus::Finished), 100);
        assert_eq!(m.blocks().map(|b| b.size).sum::<u64>(), 1000);
    }

    #[test]
    fn finds_next_block_of_a_status() {
        let mut m = BlockMap::new(1000);
        m.mark(0, 100, BlockStatus::Finished);
        m.mark(200, 100, BlockStatus::Bad);
        let b = m.next_block_with(0, BlockStatus::Bad).expect("bad block");
        assert_eq!(b.pos, 200);
        assert_eq!(b.size, 100);
        // Starting mid-block returns the remainder only.
        let b = m.next_block_with(250, BlockStatus::Bad).expect("bad block");
        assert_eq!(b.pos, 250);
        assert_eq!(b.size, 50);
    }

    #[test]
    fn roundtrips_through_the_ddrescue_format() {
        let mut m = BlockMap::new(4096);
        m.mark(0, 1024, BlockStatus::Finished);
        m.mark(1024, 512, BlockStatus::Bad);
        m.mark(1536, 512, BlockStatus::NonTrimmed);
        m.current_pos = 2048;
        m.current_pass = 3;
        m.current_status = BlockStatus::NonScraped;

        let dir = std::env::temp_dir().join("rc-image-map-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.map");
        m.write_to(&path).unwrap();

        let back = BlockMap::read_from(&path).unwrap();
        assert_eq!(back.size(), 4096);
        assert_eq!(back.current_pos, 2048);
        assert_eq!(back.current_pass, 3);
        assert_eq!(back.current_status, BlockStatus::NonScraped);
        assert_eq!(back.bytes_in(BlockStatus::Finished), 1024);
        assert_eq!(back.bytes_in(BlockStatus::Bad), 512);
        assert_eq!(back.bytes_in(BlockStatus::NonTrimmed), 512);
        assert_eq!(back.bytes_in(BlockStatus::NonTried), 2048);
    }

    #[test]
    fn status_characters_round_trip() {
        for s in [
            BlockStatus::NonTried,
            BlockStatus::NonTrimmed,
            BlockStatus::NonScraped,
            BlockStatus::Bad,
            BlockStatus::Finished,
        ] {
            assert_eq!(BlockStatus::from_char(s.as_char()), Some(s));
        }
        assert_eq!(BlockStatus::from_char('x'), None);
    }
}
