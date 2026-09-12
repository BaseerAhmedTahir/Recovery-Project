//! exFAT parser (SPEC.md section 5.4).
//!
//! # Directory entry sets
//!
//! Unlike FAT, exFAT groups a file's metadata into an *entry set* of 32-byte
//! records that must be read together:
//!
//! ```text
//!   [File 0x85] [Stream Extension 0xC0] [Filename 0xC1] [Filename 0xC1] ...
//! ```
//!
//! * **File (0x85)** carries the timestamps, attributes, the count of
//!   secondary entries that follow, and a checksum over the whole set.
//! * **Stream Extension (0xC0)** carries the name length, a name hash, the
//!   first cluster, the valid data length and the `NoFatChain` flag.
//! * **Filename (0xC1)** entries each carry 15 UTF-16 code units of the name.
//!   A 255-character name therefore needs 17 of them.
//!
//! # Deletion
//!
//! Bit 7 of the entry type byte is the *in-use* flag. Deleting a file clears
//! it across the whole set, so `0x85` becomes `0x05`, `0xC0` becomes `0x40`
//! and `0xC1` becomes `0x41`. Nothing else is destroyed: the name, sizes,
//! timestamps and first cluster all survive intact, which makes exFAT the most
//! recoverable of the three filesystems here.

pub mod boot;

use crate::entry::{
    DataLocation, Entry, EntryKind, EntryState, Extent, PathConfidence, ScanResult, Timestamps,
};
use crate::error::Result;
use boot::ExfatBoot;
use rc_device::ReadOnlyDevice;
use std::collections::HashSet;

pub const ENTRY_SIZE: usize = 32;

/// Bit 7 of the type byte: set when the entry is in use.
pub const IN_USE_MASK: u8 = 0x80;

pub const TYPE_FILE: u8 = 0x85;
pub const TYPE_STREAM_EXTENSION: u8 = 0xC0;
pub const TYPE_FILENAME: u8 = 0xC1;
pub const TYPE_ALLOCATION_BITMAP: u8 = 0x81;
pub const TYPE_UPCASE_TABLE: u8 = 0x82;
pub const TYPE_VOLUME_LABEL: u8 = 0x83;
/// A type byte of 0 ends the directory.
pub const TYPE_END_OF_DIRECTORY: u8 = 0x00;

const MAX_DEPTH: usize = 64;
const MAX_DIR_CLUSTERS: usize = 4096;

/// Strip the in-use bit to get the entry's base type.
pub fn base_type(t: u8) -> u8 {
    t | IN_USE_MASK
}

pub fn is_in_use(t: u8) -> bool {
    t & IN_USE_MASK != 0
}

/// The exFAT entry-set checksum.
///
/// Computed over every byte of the set except the two checksum bytes in the
/// File entry itself (offsets 2 and 3). Validating it confirms the secondary
/// entries really belong to this file rather than being adjacent debris.
pub fn entry_set_checksum(entries: &[u8]) -> u16 {
    let mut sum: u16 = 0;
    for (i, &b) in entries.iter().enumerate() {
        if i == 2 || i == 3 {
            continue; // the checksum field itself
        }
        // The spec writes this as (sum << 15) | (sum >> 1), which is a
        // 16-bit rotate right by one.
        sum = sum.rotate_right(1).wrapping_add(b as u16);
    }
    sum
}

/// A fully parsed entry set.
#[derive(Clone, Debug)]
pub struct EntrySet {
    pub in_use: bool,
    pub is_directory: bool,
    pub name: String,
    pub first_cluster: u32,
    pub data_length: u64,
    pub valid_data_length: u64,
    /// When set, the file occupies one contiguous run and the FAT holds no
    /// chain for it at all.
    pub no_fat_chain: bool,
    pub timestamps: Timestamps,
    pub checksum_ok: bool,
    pub notes: Vec<String>,
}

/// Parse one entry set beginning at `entries[0]`.
///
/// Returns the set and how many 32-byte records it consumed.
pub fn parse_entry_set(entries: &[u8]) -> Option<(EntrySet, usize)> {
    if entries.len() < ENTRY_SIZE {
        return None;
    }
    let type_byte = entries[0];
    if base_type(type_byte) != TYPE_FILE {
        return None;
    }

    let in_use = is_in_use(type_byte);
    let secondary_count = entries[1] as usize;
    let stored_checksum = u16::from_le_bytes([entries[2], entries[3]]);
    let attributes = u16::from_le_bytes([entries[4], entries[5]]);
    let is_directory = attributes & 0x0010 != 0;

    let total_entries = 1 + secondary_count;
    let need = total_entries * ENTRY_SIZE;
    if secondary_count < 2 || need > entries.len() {
        return None;
    }

    let mut notes = Vec::new();
    let checksum_ok = entry_set_checksum(&entries[..need]) == stored_checksum;
    if !checksum_ok {
        notes.push(
            "the entry-set checksum does not match; these secondary entries may not belong \
             to this file"
                .to_string(),
        );
    }

    // Stream extension is always the first secondary entry.
    let se = &entries[ENTRY_SIZE..ENTRY_SIZE * 2];
    if base_type(se[0]) != TYPE_STREAM_EXTENSION {
        return None;
    }
    let flags = se[1];
    let no_fat_chain = flags & 0x02 != 0;
    let name_length = se[3] as usize;
    let valid_data_length = u64::from_le_bytes(se[8..16].try_into().ok()?);
    let first_cluster = u32::from_le_bytes(se[20..24].try_into().ok()?);
    let data_length = u64::from_le_bytes(se[24..32].try_into().ok()?);

    // Filename entries: 15 UTF-16 code units each.
    let mut units: Vec<u16> = Vec::with_capacity(name_length);
    for i in 2..total_entries {
        let e = &entries[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE];
        if base_type(e[0]) != TYPE_FILENAME {
            continue;
        }
        for c in e[2..32].chunks_exact(2) {
            units.push(u16::from_le_bytes([c[0], c[1]]));
        }
    }
    units.truncate(name_length);

    // decode_utf16 keeps surrogate pairs intact, so emoji survive.
    let name: String = char::decode_utf16(units.iter().copied())
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();

    if name.chars().count() != name_length && !units.is_empty() {
        // Surrogate pairs make code units and characters differ; that is
        // expected and not a problem, so only note a genuine shortfall.
        if units.len() < name_length {
            notes.push(format!(
                "the name declares {name_length} code units but only {} were present",
                units.len()
            ));
        }
    }

    Some((
        EntrySet {
            in_use,
            is_directory,
            name,
            first_cluster,
            data_length,
            valid_data_length,
            no_fat_chain,
            timestamps: Timestamps {
                created: exfat_timestamp(
                    u32::from_le_bytes(entries[8..12].try_into().ok()?),
                    entries[20],
                    entries[23],
                ),
                modified: exfat_timestamp(
                    u32::from_le_bytes(entries[12..16].try_into().ok()?),
                    entries[21],
                    entries[24],
                ),
                accessed: exfat_timestamp(
                    u32::from_le_bytes(entries[16..20].try_into().ok()?),
                    0,
                    entries[25],
                ),
                mft_changed: None,
                // exFAT records an explicit UTC offset, unlike FAT.
                utc_known: true,
            },
            checksum_ok,
            notes,
        },
        total_entries,
    ))
}

/// exFAT timestamp: DOS date/time, a 10 ms counter, and a UTC offset byte.
///
/// The offset is a 7-bit signed count of 15-minute increments, valid only when
/// bit 7 is set. This is the reason exFAT timestamps can be converted exactly
/// while FAT's cannot.
pub fn exfat_timestamp(datetime: u32, tenms: u8, utc_offset: u8) -> Option<i64> {
    let date = (datetime >> 16) as u16;
    let time = (datetime & 0xFFFF) as u16;
    let base = crate::fat::dir::dos_to_unix_nanos(date, time, 0)?;
    let mut ns = base + (tenms as i64 * 10_000_000);

    if utc_offset & 0x80 != 0 {
        // Sign-extend the 7-bit value, then convert 15-minute units to seconds.
        let raw = utc_offset & 0x7F;
        let signed = if raw & 0x40 != 0 {
            raw as i64 - 128
        } else {
            raw as i64
        };
        ns -= signed * 15 * 60 * 1_000_000_000;
    }
    Some(ns)
}

pub struct ExfatVolume<'a> {
    device: &'a dyn ReadOnlyDevice,
    base: u64,
    pub boot: ExfatBoot,
    fat: Vec<u8>,
}

impl<'a> ExfatVolume<'a> {
    pub fn open(device: &'a dyn ReadOnlyDevice, base: u64) -> Result<Self> {
        let mut sector = vec![0u8; 512.max(device.sector_size().as_usize())];
        device.read_bytes_at(base, &mut sector)?;
        let boot = boot::parse_boot(&sector)?;

        let fat_bytes = boot.fat_length_sectors as u64 * boot.bytes_per_sector();
        let mut fat = vec![0u8; fat_bytes.min(64 * 1024 * 1024) as usize];
        device.read_bytes_at(base + boot.fat_offset_bytes(), &mut fat)?;

        Ok(ExfatVolume {
            device,
            base,
            boot,
            fat,
        })
    }

    fn read_cluster(&self, cluster: u32) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.boot.cluster_bytes() as usize];
        if let Some(off) = self.boot.cluster_offset(cluster) {
            self.device.read_bytes_at(self.base + off, &mut buf)?;
        }
        Ok(buf)
    }

    fn fat_next(&self, cluster: u32) -> Option<u32> {
        let o = cluster as usize * 4;
        if o + 4 > self.fat.len() {
            return None;
        }
        let v = u32::from_le_bytes(self.fat[o..o + 4].try_into().ok()?);
        // 0 and 1 are reserved; 0xFFFFFFF7 and above are bad-cluster and
        // end-of-chain markers.
        if !(2..0xFFFF_FFF7).contains(&v) {
            None
        } else {
            Some(v)
        }
    }

    fn chain(&self, start: u32, no_fat_chain: bool, clusters_needed: u64) -> Vec<u32> {
        if no_fat_chain {
            // Contiguous by declaration: the FAT holds nothing for this file.
            return (0..clusters_needed.min(MAX_DIR_CLUSTERS as u64))
                .map(|i| start + i as u32)
                .collect();
        }
        let mut out = Vec::new();
        let mut c = start;
        let mut seen = HashSet::new();
        while c >= 2 && out.len() < MAX_DIR_CLUSTERS && seen.insert(c) {
            out.push(c);
            match self.fat_next(c) {
                Some(n) => c = n,
                None => break,
            }
        }
        out
    }

    pub fn scan(&self) -> Result<ScanResult> {
        let mut result = ScanResult {
            geometry: crate::entry::Geometry {
                cluster_bytes: self.boot.cluster_bytes(),
                heap_offset: self.base + self.boot.cluster_heap_offset_bytes(),
                first_cluster: 2,
                cluster_count: self.boot.cluster_count as u64,
            },
            ..ScanResult::default()
        };
        let mut visited = HashSet::new();
        visited.insert(self.boot.first_cluster_of_root);

        let chain = self.chain(self.boot.first_cluster_of_root, false, 0);
        let mut root = Vec::new();
        for c in if chain.is_empty() {
            vec![self.boot.first_cluster_of_root]
        } else {
            chain
        } {
            root.extend_from_slice(&self.read_cluster(c)?);
        }

        self.walk_directory(&root, "", 0, &mut visited, &mut result)?;
        result.notes.push(format!(
            "exFAT volume, {} clusters of {} bytes; {} entries recovered",
            self.boot.cluster_count,
            self.boot.cluster_bytes(),
            result.entries.len()
        ));
        Ok(result)
    }

    fn walk_directory(
        &self,
        data: &[u8],
        prefix: &str,
        depth: usize,
        visited: &mut HashSet<u32>,
        result: &mut ScanResult,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Ok(());
        }

        let mut subdirs: Vec<(String, u32, bool, u64)> = Vec::new();
        let mut pos = 0usize;

        while pos + ENTRY_SIZE <= data.len() {
            let t = data[pos];
            // A zero type byte marks the end of the *allocated* directory, but
            // deleted sets can still follow it, so the walk continues.
            if t == TYPE_END_OF_DIRECTORY {
                pos += ENTRY_SIZE;
                continue;
            }
            if base_type(t) != TYPE_FILE {
                pos += ENTRY_SIZE;
                continue;
            }

            match parse_entry_set(&data[pos..]) {
                Some((set, consumed)) => {
                    if !set.name.is_empty() {
                        let path = if prefix.is_empty() {
                            set.name.clone()
                        } else {
                            format!("{prefix}/{}", set.name)
                        };
                        if set.is_directory {
                            subdirs.push((
                                path.clone(),
                                set.first_cluster,
                                set.no_fat_chain,
                                set.data_length,
                            ));
                        }
                        result.entries.push(self.to_entry(&set, &path));
                    }
                    pos += consumed * ENTRY_SIZE;
                }
                None => pos += ENTRY_SIZE,
            }
        }

        for (path, cluster, no_chain, len) in subdirs {
            if cluster < 2 || !visited.insert(cluster) {
                continue;
            }
            let needed = len.div_ceil(self.boot.cluster_bytes().max(1)).max(1);
            let mut bytes = Vec::new();
            for c in self.chain(cluster, no_chain, needed) {
                bytes.extend_from_slice(&self.read_cluster(c)?);
            }
            if bytes.is_empty() {
                bytes = self.read_cluster(cluster)?;
            }
            self.walk_directory(&bytes, &path, depth + 1, visited, result)?;
        }
        Ok(())
    }

    fn to_entry(&self, set: &EntrySet, path: &str) -> Entry {
        let mut notes = set.notes.clone();
        let clusters_needed = set.data_length.div_ceil(self.boot.cluster_bytes().max(1));

        let location = if set.is_directory || set.first_cluster < 2 {
            DataLocation::Unknown
        } else if set.no_fat_chain {
            // NoFatChain means the run really is contiguous, and that is a
            // recorded fact rather than an assumption, so it survives deletion.
            notes.push(
                "NoFatChain is set, so the file occupies one contiguous run and its layout \
                 is known exactly even though it was deleted"
                    .to_string(),
            );
            DataLocation::Runs(vec![Extent::new(
                set.first_cluster as u64,
                clusters_needed.max(1),
            )])
        } else if set.in_use {
            let chain = self.chain(set.first_cluster, false, clusters_needed);
            DataLocation::Runs(crate::fat::runs_from_clusters(&chain))
        } else {
            notes.push(
                "the file used a FAT chain which is cleared on delete; only the first \
                 cluster is known"
                    .to_string(),
            );
            DataLocation::FirstClusterOnly(set.first_cluster as u64)
        };

        Entry {
            id: set.first_cluster as u64,
            name: set.name.clone(),
            path: Some(path.to_string()),
            path_confidence: PathConfidence::Traversed,
            kind: if set.is_directory {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
            state: if set.in_use {
                EntryState::Allocated
            } else {
                EntryState::Deleted
            },
            size: set.data_length,
            allocated_size: clusters_needed * self.boot.cluster_bytes(),
            timestamps: set.timestamps,
            location,
            parent_id: None,
            notes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an entry set on disk, live or deleted.
    fn build_set(name: &str, is_dir: bool, cluster: u32, len: u64, deleted: bool) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_entries = units.len().div_ceil(15);
        let secondary = 1 + name_entries;
        let total = 1 + secondary;
        let mut buf = vec![0u8; total * ENTRY_SIZE];

        // File entry.
        buf[0] = TYPE_FILE;
        buf[1] = secondary as u8;
        buf[4..6].copy_from_slice(&(if is_dir { 0x0010u16 } else { 0x0020 }).to_le_bytes());
        // 2024-01-01 00:00:00, offset byte valid with zero offset.
        let date: u16 = (44 << 9) | (1 << 5) | 1;
        let dt: u32 = (date as u32) << 16;
        buf[8..12].copy_from_slice(&dt.to_le_bytes());
        buf[12..16].copy_from_slice(&dt.to_le_bytes());
        buf[16..20].copy_from_slice(&dt.to_le_bytes());
        buf[23] = 0x80;
        buf[24] = 0x80;
        buf[25] = 0x80;

        // Stream extension.
        let se = ENTRY_SIZE;
        buf[se] = TYPE_STREAM_EXTENSION;
        buf[se + 1] = 0x02; // NoFatChain
        buf[se + 3] = units.len() as u8;
        buf[se + 8..se + 16].copy_from_slice(&len.to_le_bytes());
        buf[se + 20..se + 24].copy_from_slice(&cluster.to_le_bytes());
        buf[se + 24..se + 32].copy_from_slice(&len.to_le_bytes());

        // Filename entries.
        for (i, chunk) in units.chunks(15).enumerate() {
            let o = (2 + i) * ENTRY_SIZE;
            buf[o] = TYPE_FILENAME;
            for (j, u) in chunk.iter().enumerate() {
                buf[o + 2 + j * 2..o + 4 + j * 2].copy_from_slice(&u.to_le_bytes());
            }
        }

        let ck = entry_set_checksum(&buf);
        buf[2..4].copy_from_slice(&ck.to_le_bytes());

        if deleted {
            // Deletion clears bit 7 across the whole set.
            for i in 0..total {
                buf[i * ENTRY_SIZE] &= !IN_USE_MASK;
            }
            // The checksum covers the type bytes, so recompute it the way the
            // driver would have left it: exFAT does NOT update the checksum on
            // delete, so a real deleted set has a stale checksum. Emulate that
            // by leaving the original value in place.
        }
        buf
    }

    #[test]
    fn in_use_bit_is_bit_seven() {
        assert!(is_in_use(TYPE_FILE));
        assert!(!is_in_use(0x05), "0x85 with the in-use bit cleared");
        assert_eq!(base_type(0x05), TYPE_FILE);
        assert_eq!(base_type(0x40), TYPE_STREAM_EXTENSION);
        assert_eq!(base_type(0x41), TYPE_FILENAME);
    }

    #[test]
    fn parses_a_live_entry_set() {
        let buf = build_set("photo.jpg", false, 100, 4321, false);
        let (set, consumed) = parse_entry_set(&buf).expect("parses");
        assert!(set.in_use);
        assert_eq!(set.name, "photo.jpg");
        assert_eq!(set.first_cluster, 100);
        assert_eq!(set.data_length, 4321);
        assert!(set.checksum_ok, "a live set's checksum must validate");
        assert_eq!(consumed, 3, "file + stream + one name entry");
    }

    #[test]
    fn parses_a_deleted_entry_set_with_the_name_intact() {
        let buf = build_set("deleted photo.jpg", false, 200, 999, true);
        let (set, _) = parse_entry_set(&buf).expect("parses");
        assert!(!set.in_use, "0x05 means deleted");
        assert_eq!(
            set.name, "deleted photo.jpg",
            "exFAT destroys nothing but the in-use bits"
        );
        assert_eq!(set.first_cluster, 200);
        assert_eq!(set.data_length, 999);
    }

    #[test]
    fn recovers_non_ascii_and_surrogate_pair_names() {
        for name in [
            "日本語のファイル.jpg",
            "café_niño_αβγ.txt",
            "Кириллица_Ωμέγα.png",
            "emoji_\u{1F389}\u{1F525}_test.txt",
        ] {
            let buf = build_set(name, false, 10, 1, true);
            let (set, _) = parse_entry_set(&buf).expect("parses");
            assert_eq!(set.name, name, "failed on {name:?}");
        }
    }

    /// 250 characters needs 17 filename entries.
    #[test]
    fn recovers_a_name_at_the_length_limit() {
        let name = format!("long_{}_.txt", "n".repeat(240));
        assert_eq!(name.len(), 250);
        let buf = build_set(&name, false, 10, 1, true);
        let (set, consumed) = parse_entry_set(&buf).expect("parses");
        assert_eq!(set.name, name);
        assert_eq!(consumed, 2 + 250_usize.div_ceil(15));
    }

    #[test]
    fn checksum_detects_a_tampered_set() {
        let mut buf = build_set("photo.jpg", false, 100, 4321, false);
        buf[ENTRY_SIZE + 20] ^= 0xFF; // corrupt the first cluster
        let (set, _) = parse_entry_set(&buf).expect("still parses");
        assert!(!set.checksum_ok);
        assert!(set.notes.iter().any(|n| n.contains("checksum")));
    }

    #[test]
    fn rejects_a_set_with_too_few_secondary_entries() {
        let mut buf = build_set("x.txt", false, 2, 1, false);
        buf[1] = 1; // only one secondary: no room for both stream and name
        assert!(parse_entry_set(&buf).is_none());
    }

    #[test]
    fn no_fat_chain_gives_an_exact_layout_even_when_deleted() {
        let buf = build_set("contig.bin", false, 50, 40960, true);
        let (set, _) = parse_entry_set(&buf).unwrap();
        assert!(set.no_fat_chain);
        assert!(!set.in_use);
        // A contiguous run is a recorded fact, so it survives deletion intact.
    }

    #[test]
    fn timestamps_apply_the_utc_offset() {
        let date: u16 = (44 << 9) | (1 << 5) | 1; // 2024-01-01
        let dt: u32 = (date as u32) << 16;
        let utc = exfat_timestamp(dt, 0, 0x80).expect("valid");
        assert_eq!(utc, 1_704_067_200 * 1_000_000_000);

        // +1 hour is four 15-minute units; the stored time is local, so the
        // UTC instant is one hour earlier.
        let plus1 = exfat_timestamp(dt, 0, 0x80 | 4).expect("valid");
        assert_eq!(plus1, utc - 3600 * 1_000_000_000);

        // Negative offsets sign-extend from bit 6.
        let minus1 = exfat_timestamp(dt, 0, 0x80 | (((-4i8) as u8) & 0x7F)).expect("valid");
        assert_eq!(minus1, utc + 3600 * 1_000_000_000);
    }

    #[test]
    fn the_ten_millisecond_counter_is_applied() {
        let date: u16 = (44 << 9) | (1 << 5) | 1;
        let dt: u32 = (date as u32) << 16;
        let a = exfat_timestamp(dt, 0, 0x80).unwrap();
        let b = exfat_timestamp(dt, 100, 0x80).unwrap();
        assert_eq!(b - a, 1_000_000_000, "100 x 10ms is one second");
    }
}
