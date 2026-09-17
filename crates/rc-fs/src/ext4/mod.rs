//! ext4 parser with JBD2 journal recovery (SPEC.md 5.4, Milestone 6).
//!
//! # What deletion destroys, measured
//!
//! On the ext4 fixture (a Linux 6.x kernel, `metadata_csum`, `64bit`,
//! `flex_bg`, `dir_index`), deleting a file does two things that matter:
//!
//! - the inode's extent tree is zeroed and its size set to zero, so the live
//!   inode no longer says where the data was;
//! - the file's name does not survive in the live directory block. `debugfs ls
//!   -d` finds no deleted names there, and a byte search finds them only inside
//!   the journal area.
//!
//! So the live filesystem alone recovers nothing about a deleted file but that
//! an inode was once used. What does survive is the JBD2 journal: every metadata
//! block a transaction changed is logged whole, and the transactions before the
//! deletion hold the inode-table block with the inode intact - extents, size and
//! times - and the directory block with the name in it.
//!
//! # How the journal is read
//!
//! Not replayed onto anything (SPEC.md 4.5): read. Every block of the journal
//! is examined for a descriptor, not only the transactions after the journal's
//! start pointer, because a cleanly unmounted journal says it is empty while its
//! older transactions are still sitting in its blocks - and those older
//! transactions are the ones from before the deletion. Only transactions with a
//! commit block are used. For each filesystem block, every logged copy is kept
//! with its sequence number, and recovery takes the newest copy from before the
//! deletion: the newest inode copy that is still in use, and every name that
//! appears in any copy of a directory block but not in the live one.
//!
//! # What it cannot recover
//!
//! Anything the journal has already wrapped over. The journal is a fixed ring
//! (16 MiB on the fixture) and busy filesystems cycle it in minutes, so on a
//! real disk only recent deletions are recoverable this way. Data blocks are
//! not journalled in the default `ordered` mode, so file *content* comes from
//! the blocks the recovered extents name, which may since have been reused.

use crate::entry::{
    DataLocation, Entry, EntryKind, EntryState, Extent, Geometry, PathConfidence, ScanResult,
    Timestamps,
};
use crate::error::{FsError, Result};
use rc_device::ReadOnlyDevice;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

const MAGIC: u16 = 0xEF53;
const EXTENT_MAGIC: u16 = 0xF30A;
const JBD2_MAGIC: u32 = 0xC03B3998;
const EXTENTS_FL: u32 = 0x0008_0000;
const INDEX_FL: u32 = 0x0000_1000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
const ROOT_INODE: u32 = 2;
/// Bound on directories walked, so a corrupt volume cannot loop.
const MAX_DIRS: usize = 1 << 20;
const MAX_EXTENT_DEPTH: u32 = 5;

fn le16(d: &[u8], o: usize) -> u16 {
    d.get(o..o + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0)
}
fn le32(d: &[u8], o: usize) -> u32 {
    d.get(o..o + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(0)
}
fn be32(d: &[u8], o: usize) -> u32 {
    d.get(o..o + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
struct Super {
    block: u64,
    inodes_count: u32,
    blocks_count: u64,
    ipg: u32,
    inode_size: u64,
    journal_inum: u32,
    has_journal: bool,
}

/// A parsed inode, live or from a journal copy.
#[derive(Clone, Debug)]
struct Inode {
    mode: u16,
    size: u64,
    links: u16,
    dtime: u32,
    flags: u32,
    block: [u8; 60],
    times: Timestamps,
}

impl Inode {
    fn parse(d: &[u8]) -> Option<Inode> {
        if d.len() < 128 {
            return None;
        }
        let mut block = [0u8; 60];
        block.copy_from_slice(&d[40..100]);
        let extra = if d.len() >= 132 {
            le16(d, 128) as usize
        } else {
            0
        };
        let t = |lo: usize, ex: Option<usize>| -> Option<i64> {
            let secs = le32(d, lo) as i32 as i64;
            let (epoch, nsec) = match ex {
                Some(o) if 128 + extra >= o + 4 && d.len() >= o + 4 => {
                    let e = le32(d, o);
                    (((e & 3) as i64) << 32, (e >> 2) as i64)
                }
                _ => (0, 0),
            };
            let s = secs + epoch;
            (s != 0).then_some(s * 1_000_000_000 + nsec)
        };
        Some(Inode {
            mode: le16(d, 0),
            size: le32(d, 4) as u64 | ((le32(d, 108) as u64) << 32),
            links: le16(d, 26),
            dtime: le32(d, 20),
            flags: le32(d, 32),
            block,
            times: Timestamps {
                accessed: t(8, Some(140)),
                modified: t(16, Some(136)),
                created: if 128 + extra >= 152 {
                    t(144, Some(148))
                } else {
                    None
                },
                mft_changed: t(12, Some(132)),
                utc_known: true,
            },
        })
    }

    fn kind(&self) -> Option<EntryKind> {
        match self.mode & 0xF000 {
            0x4000 => Some(EntryKind::Directory),
            0x8000 | 0xA000 => Some(EntryKind::File),
            _ => None,
        }
    }

    /// In use: linked, not deleted, and a type we recognise.
    fn in_use(&self) -> bool {
        self.dtime == 0 && self.links > 0 && self.kind().is_some()
    }
}

/// One logged copy of a filesystem block.
#[derive(Clone, Copy, Debug)]
struct Logged {
    seq: u32,
    /// Physical block of the copy on the device.
    at: u64,
    /// The copy's first four bytes were the journal magic and were zeroed in the
    /// log; restore them when reading.
    escaped: bool,
}

pub struct Ext4Volume<'a> {
    device: &'a dyn ReadOnlyDevice,
    base: u64,
    sb: Super,
    inode_tables: Vec<u64>,
}

impl<'a> Ext4Volume<'a> {
    pub fn open(device: &'a dyn ReadOnlyDevice, base: u64) -> Result<Self> {
        let mut raw = vec![0u8; 1024];
        device.read_bytes_at(base + 1024, &mut raw)?;
        if le16(&raw, 0x38) != MAGIC {
            return Err(FsError::BadBootSector {
                detail: "no ext2/3/4 superblock magic at byte 1080".into(),
            });
        }
        let log = le32(&raw, 24);
        if log > 6 {
            return Err(FsError::BadBootSector {
                detail: format!("block size 1024<<{log} is not a valid ext4 block size"),
            });
        }
        let block = 1024u64 << log;
        let incompat = le32(&raw, 0x60);
        let is64 = incompat & 0x80 != 0;
        let desc_size = if is64 {
            (le16(&raw, 0xFE) as u64).max(32)
        } else {
            32
        };
        let blocks_count = le32(&raw, 4) as u64
            | if is64 {
                (le32(&raw, 0x150) as u64) << 32
            } else {
                0
            };
        let ipg = le32(&raw, 40);
        let bpg = le32(&raw, 32) as u64;
        let first_data_block = le32(&raw, 20) as u64;
        if ipg == 0 || bpg == 0 {
            return Err(FsError::BadBootSector {
                detail: "zero blocks or inodes per group".into(),
            });
        }
        let groups = blocks_count.saturating_sub(first_data_block).div_ceil(bpg);
        if groups == 0 || groups > (1 << 24) || (ipg as u64) * groups > u32::MAX as u64 + ipg as u64
        {
            return Err(FsError::BadBootSector {
                detail: format!("{groups} block groups is not a plausible ext4 volume"),
            });
        }
        let sb = Super {
            block,
            inodes_count: le32(&raw, 0),
            blocks_count,
            ipg,
            inode_size: (le16(&raw, 88) as u64).max(128),
            journal_inum: le32(&raw, 0xE0),
            has_journal: le32(&raw, 0x5C) & 0x4 != 0,
        };

        // Group descriptors follow the superblock's block.
        let gdt_block = first_data_block + 1;
        let mut gdt = vec![0u8; (groups * desc_size) as usize];
        device.read_bytes_at(base + gdt_block * block, &mut gdt)?;
        let inode_tables = (0..groups as usize)
            .map(|g| {
                let o = g * desc_size as usize;
                let lo = le32(&gdt, o + 8) as u64;
                let hi = if is64 && desc_size > 32 {
                    le32(&gdt, o + 0x28) as u64
                } else {
                    0
                };
                lo | (hi << 32)
            })
            .collect();
        Ok(Ext4Volume {
            device,
            base,
            sb,
            inode_tables,
        })
    }

    fn read_block(&self, b: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.sb.block as usize];
        if b < self.sb.blocks_count {
            self.device
                .read_bytes_at(self.base + b * self.sb.block, &mut buf)?;
        }
        Ok(buf)
    }

    /// The block holding inode `ino`, and the inode's offset within it.
    fn inode_location(&self, ino: u32) -> Option<(u64, usize)> {
        if ino == 0 || ino > self.sb.inodes_count {
            return None;
        }
        let idx = (ino - 1) as u64;
        let g = idx / self.sb.ipg as u64;
        let table = *self.inode_tables.get(g as usize)?;
        let byte = (idx % self.sb.ipg as u64) * self.sb.inode_size;
        Some((
            table + byte / self.sb.block,
            (byte % self.sb.block) as usize,
        ))
    }

    fn read_inode(&self, ino: u32) -> Result<Option<Inode>> {
        let Some((blk, off)) = self.inode_location(ino) else {
            return Ok(None);
        };
        let b = self.read_block(blk)?;
        Ok(Inode::parse(&b[off..off + self.sb.inode_size as usize]))
    }

    /// Map an inode's content to runs, holes included as sparse runs so logical
    /// offsets stay right.
    /// Where an inode's content is: in the inode itself (fast symlinks, inline
    /// data), or in runs.
    fn location(&self, ino: &Inode, journal: &Journal) -> Result<DataLocation> {
        let fast_symlink =
            ino.mode & 0xF000 == 0xA000 && ino.size < 60 && ino.flags & EXTENTS_FL == 0;
        if fast_symlink || ino.flags & INLINE_DATA_FL != 0 {
            let n = (ino.size as usize).min(60);
            return Ok(DataLocation::Resident(ino.block[..n].to_vec()));
        }
        Ok(match self.runs(ino, journal)? {
            Some(r) => DataLocation::Runs(r),
            None => DataLocation::Unknown,
        })
    }

    fn runs(&self, ino: &Inode, journal: &Journal) -> Result<Option<Vec<Extent>>> {
        if ino.flags & INLINE_DATA_FL != 0 {
            return Ok(None);
        }
        let blocks_needed = ino.size.div_ceil(self.sb.block);
        let mut mapped: Vec<(u64, u64, u64, bool)> = Vec::new(); // logical, len, physical, uninit
        if ino.flags & EXTENTS_FL != 0 {
            self.extent_node(&ino.block, 0, &mut mapped, journal)?;
        } else {
            self.indirect_map(&ino.block, blocks_needed, &mut mapped)?;
        }
        mapped.sort_by_key(|m| m.0);
        let mut runs = Vec::new();
        let mut next = 0u64;
        for (logical, len, phys, uninit) in mapped {
            if logical >= blocks_needed {
                break;
            }
            if logical > next {
                runs.push(Extent::sparse(logical - next));
            }
            let len = len.min(blocks_needed - logical);
            if uninit {
                runs.push(Extent::sparse(len));
            } else {
                runs.push(Extent::new(phys, len));
            }
            next = logical + len;
        }
        if next < blocks_needed {
            runs.push(Extent::sparse(blocks_needed - next));
        }
        Ok(Some(runs))
    }

    fn extent_node(
        &self,
        node: &[u8],
        depth_seen: u32,
        out: &mut Vec<(u64, u64, u64, bool)>,
        journal: &Journal,
    ) -> Result<()> {
        if le16(node, 0) != EXTENT_MAGIC || depth_seen > MAX_EXTENT_DEPTH {
            return Ok(());
        }
        let entries = le16(node, 2) as usize;
        let depth = le16(node, 6);
        for k in 0..entries {
            let e = 12 + 12 * k;
            if e + 12 > node.len() {
                break;
            }
            if depth == 0 {
                let logical = le32(node, e) as u64;
                let raw_len = le16(node, e + 4) as u64;
                let (len, uninit) = if raw_len > 32768 {
                    (raw_len - 32768, true)
                } else {
                    (raw_len, false)
                };
                let phys = ((le16(node, e + 6) as u64) << 32) | le32(node, e + 8) as u64;
                out.push((logical, len, phys, uninit));
            } else {
                let child = ((le16(node, e + 8) as u64) << 32) | le32(node, e + 4) as u64;
                let mut block = self.read_block(child)?;
                // An index block freed by the deletion may have been reused;
                // its journal copy is the one from when it was in use.
                if le16(&block, 0) != EXTENT_MAGIC {
                    if let Some(copy) =
                        journal.newest_matching(self, child, |b| le16(b, 0) == EXTENT_MAGIC)?
                    {
                        block = copy;
                    }
                }
                self.extent_node(&block, depth_seen + 1, out, journal)?;
            }
        }
        Ok(())
    }

    fn indirect_map(
        &self,
        iblock: &[u8],
        blocks_needed: u64,
        out: &mut Vec<(u64, u64, u64, bool)>,
    ) -> Result<()> {
        let mut logical = 0u64;
        for k in 0..12 {
            self.map_pointer(
                le32(iblock, 4 * k) as u64,
                0,
                &mut logical,
                blocks_needed,
                out,
            )?;
        }
        // Single, double and triple indirection.
        for (slot, level) in [(12usize, 1u32), (13, 2), (14, 3)] {
            self.map_pointer(
                le32(iblock, 4 * slot) as u64,
                level,
                &mut logical,
                blocks_needed,
                out,
            )?;
        }
        Ok(())
    }

    /// Map one block pointer at `level` levels of indirection. A zero pointer is
    /// a hole spanning everything below it, so logical offsets advance past it.
    fn map_pointer(
        &self,
        ptr: u64,
        level: u32,
        logical: &mut u64,
        blocks_needed: u64,
        out: &mut Vec<(u64, u64, u64, bool)>,
    ) -> Result<()> {
        if *logical >= blocks_needed {
            return Ok(());
        }
        let per = self.sb.block / 4;
        if ptr == 0 || ptr >= self.sb.blocks_count {
            *logical += per.pow(level);
            return Ok(());
        }
        if level == 0 {
            match out.last_mut() {
                Some(last) if last.0 + last.1 == *logical && last.2 + last.1 == ptr => last.1 += 1,
                _ => out.push((*logical, 1, ptr, false)),
            }
            *logical += 1;
            return Ok(());
        }
        let data = self.read_block(ptr)?;
        for i in 0..per as usize {
            if *logical >= blocks_needed {
                break;
            }
            self.map_pointer(
                le32(&data, 4 * i) as u64,
                level - 1,
                logical,
                blocks_needed,
                out,
            )?;
        }
        Ok(())
    }

    fn content_blocks(&self, runs: &[Extent]) -> Vec<u64> {
        let mut v = Vec::new();
        for r in runs.iter().filter(|r| !r.sparse) {
            v.extend(r.start_cluster..r.start_cluster + r.cluster_count);
        }
        v
    }

    pub fn scan(&self) -> Result<ScanResult> {
        let mut result = ScanResult {
            geometry: Geometry {
                cluster_bytes: self.sb.block,
                heap_offset: self.base,
                first_cluster: 0,
                cluster_count: self.sb.blocks_count,
            },
            ..ScanResult::default()
        };
        let journal = if self.sb.has_journal && self.sb.journal_inum != 0 {
            match Journal::read(self) {
                Ok(j) => j,
                Err(e) => {
                    result
                        .notes
                        .push(format!("the journal could not be read: {e}"));
                    Journal::default()
                }
            }
        } else {
            result
                .notes
                .push("no journal: deleted files cannot be located".into());
            Journal::default()
        };
        result.notes.push(format!(
            "journal: {} committed transaction(s), {} logged block copies",
            journal.committed.len(),
            journal.copies.values().map(Vec::len).sum::<usize>()
        ));

        // --- the live tree ---------------------------------------------------
        struct DirInfo {
            path: Option<String>,
            indexed: bool,
            /// (name, inode) of live entries, to tell deleted ones apart.
            live: HashSet<(String, u32)>,
        }
        let mut dirs: HashMap<u32, DirInfo> = HashMap::new();
        // fs block -> (directory inode, logical index within the directory)
        let mut dir_blocks: HashMap<u64, (u32, u64)> = HashMap::new();
        let mut live_inodes: HashSet<u32> = HashSet::new();
        let mut queue: VecDeque<(u32, Option<String>, Inode)> = VecDeque::new();
        if let Some(root) = self.read_inode(ROOT_INODE)? {
            queue.push_back((ROOT_INODE, Some(String::new()), root));
        }
        live_inodes.insert(ROOT_INODE);
        let mut slack_names: Vec<(u32, String, u32, u8)> = Vec::new(); // parent, name, inode, type

        while let Some((ino, path, inode)) = queue.pop_front() {
            if dirs.len() >= MAX_DIRS || dirs.contains_key(&ino) {
                continue;
            }
            let indexed = inode.flags & INDEX_FL != 0;
            let mut info = DirInfo {
                path: path.clone(),
                indexed,
                live: HashSet::new(),
            };
            let runs = self.runs(&inode, &journal)?.unwrap_or_default();
            for (logical, blk) in self.content_blocks(&runs).into_iter().enumerate() {
                dir_blocks.insert(blk, (ino, logical as u64));
                let data = self.read_block(blk)?;
                let (live, slack) =
                    parse_dir_block(&data, indexed && logical == 0, self.sb.inodes_count);
                for d in live {
                    if d.name == "." || d.name == ".." {
                        continue;
                    }
                    info.live.insert((d.name.clone(), d.inode));
                    if !live_inodes.insert(d.inode) {
                        continue; // a hard link already walked
                    }
                    let child_path = path.as_ref().map(|p| join(p, &d.name));
                    let Some(child) = self.read_inode(d.inode)? else {
                        continue;
                    };
                    match child.kind() {
                        Some(EntryKind::Directory) => {
                            result.entries.push(self.entry(
                                d.inode,
                                &d.name,
                                child_path.clone(),
                                Some(ino),
                                EntryState::Allocated,
                                &child,
                                self.location(&child, &journal)?,
                                vec![],
                            ));
                            queue.push_back((d.inode, child_path, child));
                        }
                        Some(EntryKind::File) => {
                            result.entries.push(self.entry(
                                d.inode,
                                &d.name,
                                child_path,
                                Some(ino),
                                EntryState::Allocated,
                                &child,
                                self.location(&child, &journal)?,
                                vec![],
                            ));
                        }
                        None => {}
                    }
                }
                for d in slack {
                    slack_names.push((ino, d.name, d.inode, d.file_type));
                }
            }
            dirs.insert(ino, info);
        }

        // --- names the live tree no longer has --------------------------------
        // (parent, name) -> (inode, file type, where it was found)
        let mut deleted: BTreeMap<(u32, String), (u32, u8, &'static str)> = BTreeMap::new();
        for (parent, name, inode, ty) in slack_names {
            if !dirs
                .get(&parent)
                .is_some_and(|d| d.live.contains(&(name.clone(), inode)))
            {
                deleted
                    .entry((parent, name))
                    .or_insert((inode, ty, "directory slack"));
            }
        }
        // Journal copies of directory blocks: deleted directories found this way
        // add their own blocks, so repeat until nothing new appears.
        let mut deleted_dirs_done: HashSet<u32> = HashSet::new();
        loop {
            let mut new_blocks = Vec::new();
            for (&blk, &(dir_ino, logical)) in &dir_blocks {
                let Some(copies) = journal.copies.get(&blk) else {
                    continue;
                };
                let indexed = dirs.get(&dir_ino).is_some_and(|d| d.indexed) && logical == 0;
                for c in copies {
                    let data = journal.read_copy(self, c)?;
                    let (live, slack) = parse_dir_block(&data, indexed, self.sb.inodes_count);
                    for d in live.into_iter().chain(slack) {
                        if d.name == "." || d.name == ".." {
                            continue;
                        }
                        let known = dirs
                            .get(&dir_ino)
                            .is_some_and(|x| x.live.contains(&(d.name.clone(), d.inode)));
                        if !known {
                            deleted.entry((dir_ino, d.name)).or_insert((
                                d.inode,
                                d.file_type,
                                "journal",
                            ));
                        }
                    }
                }
            }
            // Deleted directories: locate their blocks from a journal inode copy.
            for (&(parent, ref name), &(ino, ty, _)) in &deleted {
                if ty != 2 || deleted_dirs_done.contains(&ino) || live_inodes.contains(&ino) {
                    continue;
                }
                deleted_dirs_done.insert(ino);
                if let Some(copy) = self.pre_deletion_inode(ino, &journal)? {
                    if copy.kind() == Some(EntryKind::Directory) {
                        let path = dirs
                            .get(&parent)
                            .and_then(|p| p.path.as_ref())
                            .map(|p| join(p, name));
                        dirs.entry(ino).or_insert(DirInfo {
                            path,
                            indexed: copy.flags & INDEX_FL != 0,
                            live: HashSet::new(),
                        });
                        let runs = self.runs(&copy, &journal)?.unwrap_or_default();
                        for (logical, blk) in self.content_blocks(&runs).into_iter().enumerate() {
                            if let std::collections::hash_map::Entry::Vacant(v) =
                                dir_blocks.entry(blk)
                            {
                                v.insert((ino, logical as u64));
                                new_blocks.push(blk);
                            }
                        }
                    }
                }
            }
            if new_blocks.is_empty() {
                break;
            }
        }

        // --- deleted entries -------------------------------------------------
        let mut named_inodes: HashSet<u32> = HashSet::new();
        for ((parent, name), (ino, _ty, source)) in &deleted {
            if live_inodes.contains(ino) {
                // Renamed or hard-linked elsewhere, not deleted.
                continue;
            }
            named_inodes.insert(*ino);
            let parent_info = dirs.get(parent);
            let path = parent_info
                .and_then(|p| p.path.as_ref())
                .map(|p| join(p, name));
            let mut notes = vec![format!("name recovered from {source}")];
            let (inode, location) = match self.pre_deletion_inode(*ino, &journal)? {
                Some(copy) => {
                    notes.push(
                        "size, times and extents recovered from a pre-deletion copy of the \
                         inode in the journal"
                            .into(),
                    );
                    let loc = self.location(&copy, &journal)?;
                    (copy, loc)
                }
                None => {
                    notes.push(
                        "no pre-deletion copy of the inode survives in the journal; its extents \
                         were zeroed on deletion, so the content cannot be located"
                            .into(),
                    );
                    match self.read_inode(*ino)? {
                        Some(live) => (live, DataLocation::Unknown),
                        None => continue,
                    }
                }
            };
            if inode.kind().is_none() {
                continue;
            }
            let mut e = self.entry(
                *ino,
                name,
                path,
                Some(*parent),
                EntryState::Deleted,
                &inode,
                location,
                notes,
            );
            if parent_info.is_none() {
                e.path_confidence = PathConfidence::ParentUnknown;
            }
            result.entries.push(e);
        }

        // Deleted inodes nothing names any more: content may still be locatable.
        for ino in 1..=self.sb.inodes_count {
            if live_inodes.contains(&ino) || named_inodes.contains(&ino) {
                continue;
            }
            let Some(live) = self.read_inode(ino)? else {
                continue;
            };
            if live.dtime == 0 || live.mode == 0 {
                continue;
            }
            if let Some(copy) = self.pre_deletion_inode(ino, &journal)? {
                if copy.kind() != Some(EntryKind::File) {
                    continue;
                }
                let loc = self.location(&copy, &journal)?;
                let mut e = self.entry(
                    ino,
                    &format!("inode-{ino}"),
                    None,
                    None,
                    EntryState::Deleted,
                    &copy,
                    loc,
                    vec!["no name survives; extents from a pre-deletion journal copy".into()],
                );
                e.path_confidence = PathConfidence::ParentUnknown;
                result.entries.push(e);
            }
        }
        Ok(result)
    }

    /// The newest journal copy of inode `ino` from while it was in use.
    fn pre_deletion_inode(&self, ino: u32, journal: &Journal) -> Result<Option<Inode>> {
        let Some((blk, off)) = self.inode_location(ino) else {
            return Ok(None);
        };
        let Some(copies) = journal.copies.get(&blk) else {
            return Ok(None);
        };
        for c in copies.iter().rev() {
            let data = journal.read_copy(self, c)?;
            if let Some(i) = Inode::parse(&data[off..off + self.sb.inode_size as usize]) {
                if i.in_use() && (i.size > 0 || i.kind() == Some(EntryKind::Directory)) {
                    return Ok(Some(i));
                }
            }
        }
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    fn entry(
        &self,
        ino: u32,
        name: &str,
        path: Option<String>,
        parent: Option<u32>,
        state: EntryState,
        inode: &Inode,
        location: DataLocation,
        notes: Vec<String>,
    ) -> Entry {
        let kind = inode.kind().unwrap_or(EntryKind::File);
        let allocated = match &location {
            DataLocation::Runs(r) => {
                r.iter()
                    .filter(|e| !e.sparse)
                    .map(|e| e.cluster_count)
                    .sum::<u64>()
                    * self.sb.block
            }
            _ => 0,
        };
        Entry {
            id: ino as u64,
            name: name.to_string(),
            path,
            path_confidence: PathConfidence::Traversed,
            kind,
            state,
            size: if kind == EntryKind::File {
                inode.size
            } else {
                0
            },
            allocated_size: allocated,
            timestamps: inode.times,
            location,
            parent_id: parent.map(|p| p as u64),
            notes,
        }
    }
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

struct Dirent {
    inode: u32,
    name: String,
    file_type: u8,
}

/// Live entries and the deleted entries found in their slack.
///
/// `dx_root` is block 0 of a hashed directory, whose second entry's slack holds
/// the hash index rather than old names, so its slack is not read.
fn parse_dir_block(d: &[u8], dx_root: bool, inodes: u32) -> (Vec<Dirent>, Vec<Dirent>) {
    let mut live = Vec::new();
    let mut slack = Vec::new();
    let bs = d.len();
    // An interior hash-tree node: one empty entry covering the block.
    if le32(d, 0) == 0 && le16(d, 4) as usize == bs {
        return (live, slack);
    }
    let mut off = 0usize;
    let mut first = true;
    while off + 12 <= bs {
        let inode = le32(d, off);
        let rec_len = le16(d, off + 4) as usize;
        let name_len = d[off + 6] as usize;
        let file_type = d[off + 7];
        if rec_len < 12 || off + rec_len > bs || rec_len % 4 != 0 {
            break;
        }
        let real = (8 + name_len).div_ceil(4) * 4;
        let is_tail = inode == 0 && rec_len == 12 && name_len == 0 && file_type == 0xDE;
        if inode != 0 && name_len > 0 && 8 + name_len <= rec_len && !is_tail {
            if let Some(name) = valid_name(&d[off + 8..off + 8 + name_len]) {
                live.push(Dirent {
                    inode,
                    name,
                    file_type,
                });
            }
        }
        let skip_slack = dx_root && !first;
        if !skip_slack && rec_len > real + 12 {
            scan_slack(&d[off + real.max(12)..off + rec_len], inodes, &mut slack);
        }
        first = false;
        off += rec_len;
    }
    (live, slack)
}

fn scan_slack(s: &[u8], inodes: u32, out: &mut Vec<Dirent>) {
    let mut p = 0usize;
    while p + 12 <= s.len() {
        let inode = le32(s, p);
        let name_len = s[p + 6] as usize;
        let file_type = s[p + 7];
        if inode != 0
            && inode <= inodes
            && name_len > 0
            && p + 8 + name_len <= s.len()
            && file_type <= 7
        {
            if let Some(name) = valid_name(&s[p + 8..p + 8 + name_len]) {
                out.push(Dirent {
                    inode,
                    name,
                    file_type,
                });
                p += (8 + name_len).div_ceil(4) * 4;
                continue;
            }
        }
        p += 4;
    }
}

fn valid_name(b: &[u8]) -> Option<String> {
    if b.iter().any(|&c| c == 0 || c == b'/') {
        return None;
    }
    let s = std::str::from_utf8(b).ok()?;
    if s.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(s.to_string())
}

// ---------------------------------------------------------------------------
// the journal
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Journal {
    /// fs block -> copies, oldest transaction first.
    copies: HashMap<u64, Vec<Logged>>,
    committed: HashSet<u32>,
}

impl Journal {
    fn read(vol: &Ext4Volume) -> Result<Journal> {
        let Some(inode) = vol.read_inode(vol.sb.journal_inum)? else {
            return Ok(Journal::default());
        };
        let runs = vol.runs(&inode, &Journal::default())?.unwrap_or_default();
        let phys = vol.content_blocks(&runs);
        if phys.is_empty() {
            return Ok(Journal::default());
        }
        let sb = vol.read_block(phys[0])?;
        if be32(&sb, 0) != JBD2_MAGIC || !matches!(be32(&sb, 4), 3 | 4) {
            return Ok(Journal::default());
        }
        let incompat = if be32(&sb, 4) == 4 {
            be32(&sb, 0x28)
        } else {
            0
        };
        let is64 = incompat & 0x2 != 0;
        let csum2 = incompat & 0x8 != 0;
        let csum3 = incompat & 0x10 != 0;
        let tag_bytes = if csum3 {
            16
        } else {
            let base = 12 + if csum2 { 2 } else { 0 };
            if is64 {
                base
            } else {
                base - 4
            }
        };
        let tail = if csum2 || csum3 { 4 } else { 0 };
        let bs = vol.sb.block as usize;
        let maxlen = phys.len();

        let mut pending: Vec<(u32, u64, u64, bool)> = Vec::new(); // seq, fs block, jblock, escaped
        let mut committed = HashSet::new();
        let mut k = 1usize;
        while k < maxlen {
            let b = vol.read_block(phys[k])?;
            if be32(&b, 0) != JBD2_MAGIC {
                k += 1;
                continue;
            }
            let seq = be32(&b, 8);
            match be32(&b, 4) {
                1 => {
                    let mut p = 12usize;
                    let mut data = k + 1;
                    while p + tag_bytes <= bs - tail && data < maxlen {
                        let lo = be32(&b, p) as u64;
                        let (flags, hi) = if csum3 {
                            (be32(&b, p + 4), be32(&b, p + 8) as u64)
                        } else {
                            (
                                u16::from_be_bytes([b[p + 6], b[p + 7]]) as u32,
                                if is64 { be32(&b, p + 8) as u64 } else { 0 },
                            )
                        };
                        pending.push((seq, (hi << 32) | lo, phys[data], flags & 1 != 0));
                        data += 1;
                        p += tag_bytes;
                        if flags & 2 == 0 {
                            p += 16; // UUID follows unless SAME_UUID
                        }
                        if flags & 8 != 0 {
                            break; // LAST_TAG
                        }
                    }
                    k = data;
                }
                2 => {
                    committed.insert(seq);
                    k += 1;
                }
                _ => k += 1,
            }
        }
        let mut copies: HashMap<u64, Vec<Logged>> = HashMap::new();
        for (seq, blk, at, escaped) in pending {
            if committed.contains(&seq) {
                copies
                    .entry(blk)
                    .or_default()
                    .push(Logged { seq, at, escaped });
            }
        }
        for v in copies.values_mut() {
            v.sort_by_key(|c| c.seq);
        }
        Ok(Journal { copies, committed })
    }

    fn read_copy(&self, vol: &Ext4Volume, c: &Logged) -> Result<Vec<u8>> {
        let mut b = vol.read_block(c.at)?;
        if c.escaped {
            b[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        }
        Ok(b)
    }

    fn newest_matching(
        &self,
        vol: &Ext4Volume,
        blk: u64,
        ok: impl Fn(&[u8]) -> bool,
    ) -> Result<Option<Vec<u8>>> {
        if let Some(copies) = self.copies.get(&blk) {
            for c in copies.iter().rev() {
                let b = self.read_copy(vol, c)?;
                if ok(&b) {
                    return Ok(Some(b));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirent(inode: u32, rec_len: u16, name: &str, ty: u8) -> Vec<u8> {
        let mut v = inode.to_le_bytes().to_vec();
        v.extend_from_slice(&rec_len.to_le_bytes());
        v.push(name.len() as u8);
        v.push(ty);
        v.extend_from_slice(name.as_bytes());
        v.resize(rec_len as usize, 0);
        v
    }

    #[test]
    fn a_deleted_entry_in_slack_is_found() {
        // ".", then "a.txt" whose rec_len runs to the end of the block and
        // covers a deleted "gone.jpg" left behind it.
        let mut block = dirent(11, 12, ".", 2);
        let mut live = dirent(12, 16, "a.txt", 1);
        live[4..6].copy_from_slice(&(4096u16 - 12).to_le_bytes());
        block.extend_from_slice(&live);
        block.extend_from_slice(&dirent(13, 16, "gone.jpg", 1));
        block.resize(4096, 0);
        let (live, slack) = parse_dir_block(&block, false, 100);
        assert_eq!(
            live.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            vec![".", "a.txt"]
        );
        assert_eq!(slack.len(), 1);
        assert_eq!((slack[0].name.as_str(), slack[0].inode), ("gone.jpg", 13));
    }

    #[test]
    fn a_hash_tree_interior_node_yields_nothing() {
        let mut block = vec![0u8; 4096];
        block[4..6].copy_from_slice(&4096u16.to_le_bytes());
        let (live, slack) = parse_dir_block(&block, false, 100);
        assert!(live.is_empty() && slack.is_empty());
    }

    #[test]
    fn names_with_nul_or_slash_are_not_names() {
        assert!(valid_name(b"ok.txt").is_some());
        assert!(valid_name(b"a/b").is_none());
        assert!(valid_name(b"a\0b").is_none());
    }
}
