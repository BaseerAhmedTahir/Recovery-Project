//! Partition table and partition types shared by the MBR, GPT and
//! signature-scan discovery paths.

use rc_device::Lba;
use std::fmt;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Filesystem guessed from a partition's type code or from its boot sector.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum FsKind {
    Ntfs,
    Fat12,
    Fat16,
    Fat32,
    ExFat,
    Ext4,
    Apfs,
    HfsPlus,
    /// Recognised as *something*, but not one we parse.
    Other,
    Unknown,
}

impl FsKind {
    /// True for filesystems `rc-fs` can currently parse.
    pub fn is_supported(self) -> bool {
        matches!(
            self,
            FsKind::Ntfs | FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 | FsKind::ExFat
        )
    }
}

impl fmt::Display for FsKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FsKind::Ntfs => "ntfs",
            FsKind::Fat12 => "fat12",
            FsKind::Fat16 => "fat16",
            FsKind::Fat32 => "fat32",
            FsKind::ExFat => "exfat",
            FsKind::Ext4 => "ext4",
            FsKind::Apfs => "apfs",
            FsKind::HfsPlus => "hfs+",
            FsKind::Other => "other",
            FsKind::Unknown => "unknown",
        })
    }
}

/// How a partition came to be known.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Origin {
    /// Read from a valid MBR partition entry.
    Mbr,
    /// Read from the primary GPT.
    Gpt,
    /// Read from the backup GPT after the primary was unreadable.
    GptBackup,
    /// Reconstructed by scanning for a filesystem boot sector or superblock,
    /// because no usable partition table was found. Never written to disk.
    Reconstructed,
    /// The whole device is one filesystem with no partition table at all.
    WholeDevice,
}

impl Origin {
    /// True when this partition was inferred rather than read from a table.
    /// The CLI marks these so the operator knows the difference.
    pub fn is_inferred(self) -> bool {
        matches!(self, Origin::Reconstructed | Origin::WholeDevice)
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Origin::Mbr => "mbr",
            Origin::Gpt => "gpt",
            Origin::GptBackup => "gpt-backup",
            Origin::Reconstructed => "reconstructed",
            Origin::WholeDevice => "whole-device",
        })
    }
}

/// One partition, wherever it came from.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Partition {
    /// 1-based index for display. Reconstructed partitions are numbered in
    /// discovery order.
    pub index: usize,
    pub start: Lba,
    /// Length in sectors. Zero means "unknown", which happens for a
    /// reconstructed partition whose filesystem does not record its own size.
    pub sectors: u64,
    pub fs: FsKind,
    pub origin: Origin,
    /// GPT partition name, or an MBR type byte rendered as hex.
    pub label: Option<String>,
    /// GPT type GUID, when it came from a GPT.
    pub type_guid: Option<String>,
    pub bootable: bool,
    /// Confidence 0-100 for reconstructed partitions. Table-derived
    /// partitions are 100.
    pub confidence: u8,
    /// Why this partition is believed to exist, for the UI to explain itself.
    pub evidence: Vec<String>,
}

impl Partition {
    pub fn end(&self) -> Lba {
        Lba(self.start.0 + self.sectors)
    }

    pub fn byte_offset(&self, sector_size: rc_device::SectorSize) -> u64 {
        self.start.byte_offset(sector_size)
    }

    pub fn byte_len(&self, sector_size: rc_device::SectorSize) -> u64 {
        self.sectors * sector_size.get() as u64
    }

    /// True if this partition's extent overlaps `other`'s.
    pub fn overlaps(&self, other: &Partition) -> bool {
        self.sectors > 0
            && other.sectors > 0
            && self.start.0 < other.end().0
            && other.start.0 < self.end().0
    }
}

/// Everything discovered about a device's partitioning.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct PartitionTable {
    pub partitions: Vec<Partition>,
    /// A protective MBR was present, i.e. the device is GPT-formatted.
    pub protective_mbr: bool,
    /// The primary GPT header was valid.
    pub primary_gpt_ok: bool,
    /// The backup GPT header was valid.
    pub backup_gpt_ok: bool,
    /// Notes for the operator: recovered-from-backup, CRC mismatches, and the
    /// reason a scan was needed.
    pub notes: Vec<String>,
}

impl PartitionTable {
    /// Partitions holding a filesystem `rc-fs` can parse.
    pub fn supported(&self) -> impl Iterator<Item = &Partition> {
        self.partitions.iter().filter(|p| p.fs.is_supported())
    }

    pub fn is_empty(&self) -> bool {
        self.partitions.is_empty()
    }

    /// Sort by start LBA and renumber, so display order matches disk order.
    pub fn normalise(&mut self) {
        self.partitions.sort_by_key(|p| (p.start.0, p.sectors));
        for (i, p) in self.partitions.iter_mut().enumerate() {
            p.index = i + 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(start: u64, sectors: u64) -> Partition {
        Partition {
            index: 0,
            start: Lba(start),
            sectors,
            fs: FsKind::Ntfs,
            origin: Origin::Reconstructed,
            label: None,
            type_guid: None,
            bootable: false,
            confidence: 50,
            evidence: vec![],
        }
    }

    #[test]
    fn overlap_detection() {
        let a = part(0, 100);
        let b = part(50, 100);
        let c = part(100, 100);
        assert!(a.overlaps(&b), "50..150 overlaps 0..100");
        assert!(!a.overlaps(&c), "100..200 starts exactly where 0..100 ends");
        assert!(b.overlaps(&c));
    }

    #[test]
    fn zero_length_partitions_never_overlap() {
        // A reconstructed partition of unknown length must not be treated as
        // colliding with everything on the disk.
        let unknown = part(10, 0);
        assert!(!unknown.overlaps(&part(0, 100)));
        assert!(!part(0, 100).overlaps(&unknown));
    }

    #[test]
    fn normalise_sorts_and_renumbers() {
        let mut t = PartitionTable {
            partitions: vec![part(2048, 100), part(0, 64), part(500, 10)],
            ..Default::default()
        };
        t.normalise();
        assert_eq!(
            t.partitions.iter().map(|p| p.start.0).collect::<Vec<_>>(),
            vec![0, 500, 2048]
        );
        assert_eq!(
            t.partitions.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn supported_filters_to_parseable_filesystems() {
        assert!(FsKind::Ntfs.is_supported());
        assert!(FsKind::Fat32.is_supported());
        assert!(FsKind::ExFat.is_supported());
        assert!(!FsKind::Ext4.is_supported(), "ext4 lands in Milestone 6");
        assert!(!FsKind::Unknown.is_supported());
    }

    #[test]
    fn inferred_origins_are_flagged() {
        assert!(Origin::Reconstructed.is_inferred());
        assert!(Origin::WholeDevice.is_inferred());
        assert!(!Origin::Gpt.is_inferred());
        assert!(!Origin::Mbr.is_inferred());
    }
}
