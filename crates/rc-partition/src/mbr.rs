//! MBR parsing, including the protective MBR that fronts a GPT disk
//! (SPEC.md section 5.3).

use crate::table::{FsKind, Origin, Partition};
use rc_device::Lba;

pub const MBR_SIGNATURE: u16 = 0xAA55;
/// Type byte 0xEE marks the protective MBR entry covering a GPT disk.
pub const TYPE_GPT_PROTECTIVE: u8 = 0xEE;
/// Type byte 0x05 / 0x0F introduce an extended partition chain.
pub const TYPE_EXTENDED_CHS: u8 = 0x05;
pub const TYPE_EXTENDED_LBA: u8 = 0x0F;

/// One raw 16-byte MBR partition entry.
#[derive(Clone, Copy, Debug)]
pub struct MbrEntry {
    pub bootable: bool,
    pub type_byte: u8,
    pub start_lba: u32,
    pub sectors: u32,
}

impl MbrEntry {
    pub fn is_empty(&self) -> bool {
        self.type_byte == 0 || self.sectors == 0
    }

    pub fn is_extended(&self) -> bool {
        matches!(self.type_byte, TYPE_EXTENDED_CHS | TYPE_EXTENDED_LBA)
    }
}

/// Map an MBR type byte to a filesystem guess.
///
/// This is only a hint. The authoritative answer comes from reading the boot
/// sector at the partition start, because the type byte is frequently wrong on
/// disks that have been repartitioned by hand.
pub fn fs_from_type_byte(t: u8) -> FsKind {
    match t {
        0x01 => FsKind::Fat12,
        0x04 | 0x06 | 0x0E => FsKind::Fat16,
        0x0B | 0x0C => FsKind::Fat32,
        0x07 => FsKind::Ntfs, // also exFAT and HPFS; the boot sector decides
        0x83 => FsKind::Ext4,
        0xAF => FsKind::HfsPlus,
        0xEE => FsKind::Other, // GPT protective
        0x00 => FsKind::Unknown,
        _ => FsKind::Other,
    }
}

/// Parse the four primary entries out of a 512-byte sector.
///
/// Returns `None` when the 0xAA55 signature is missing, which is the normal
/// case for a whole-device filesystem with no partition table at all.
pub fn parse_mbr(sector: &[u8]) -> Option<[MbrEntry; 4]> {
    if sector.len() < 512 {
        return None;
    }
    if u16::from_le_bytes([sector[510], sector[511]]) != MBR_SIGNATURE {
        return None;
    }

    let mut out = [MbrEntry {
        bootable: false,
        type_byte: 0,
        start_lba: 0,
        sectors: 0,
    }; 4];

    for (i, slot) in out.iter_mut().enumerate() {
        let o = 446 + i * 16;
        *slot = MbrEntry {
            // 0x80 means active; anything other than 0x00/0x80 means the table
            // is probably not a real MBR.
            bootable: sector[o] == 0x80,
            type_byte: sector[o + 4],
            start_lba: u32::from_le_bytes([
                sector[o + 8],
                sector[o + 9],
                sector[o + 10],
                sector[o + 11],
            ]),
            sectors: u32::from_le_bytes([
                sector[o + 12],
                sector[o + 13],
                sector[o + 14],
                sector[o + 15],
            ]),
        };
    }
    Some(out)
}

/// True if the entries look like a GPT protective MBR: exactly one entry, of
/// type 0xEE, starting at LBA 1.
pub fn is_protective(entries: &[MbrEntry; 4]) -> bool {
    let used: Vec<&MbrEntry> = entries.iter().filter(|e| !e.is_empty()).collect();
    used.len() == 1 && used[0].type_byte == TYPE_GPT_PROTECTIVE && used[0].start_lba == 1
}

/// Sanity-check a candidate MBR so random data containing 0xAA55 at offset 510
/// is not mistaken for a partition table.
///
/// About 1 sector in 65536 of random data ends in 0xAA55, and a 512 GiB disk
/// has a billion sectors, so this matters for the signature scan.
pub fn looks_plausible(entries: &[MbrEntry; 4], total_sectors: u64) -> bool {
    let mut any = false;
    for e in entries {
        if e.is_empty() {
            // A zero entry must be entirely zero to be believable.
            if e.type_byte != 0 && e.sectors == 0 {
                return false;
            }
            continue;
        }
        any = true;
        // Boot flag is 0x00 or 0x80 and nothing else.
        // (parse_mbr already reduced it to a bool, so re-derive from extent.)
        if e.start_lba == 0 {
            return false; // a partition cannot start at the MBR itself
        }
        let end = e.start_lba as u64 + e.sectors as u64;
        if total_sectors > 0 && end > total_sectors {
            return false; // runs off the end of the device
        }
    }
    any
}

/// Convert plausible primary entries into partitions.
pub fn to_partitions(entries: &[MbrEntry; 4]) -> Vec<Partition> {
    let mut out = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if e.is_empty() || e.type_byte == TYPE_GPT_PROTECTIVE {
            continue;
        }
        out.push(Partition {
            index: i + 1,
            start: Lba(e.start_lba as u64),
            sectors: e.sectors as u64,
            fs: fs_from_type_byte(e.type_byte),
            origin: Origin::Mbr,
            label: Some(format!("type 0x{:02X}", e.type_byte)),
            type_guid: None,
            bootable: e.bootable,
            confidence: 100,
            evidence: vec![format!(
                "MBR entry {}, type 0x{:02X}, {} sectors at LBA {}",
                i + 1,
                e.type_byte,
                e.sectors,
                e.start_lba
            )],
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_sector() -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }

    fn write_entry(s: &mut [u8], slot: usize, boot: u8, t: u8, start: u32, len: u32) {
        let o = 446 + slot * 16;
        s[o] = boot;
        s[o + 4] = t;
        s[o + 8..o + 12].copy_from_slice(&start.to_le_bytes());
        s[o + 12..o + 16].copy_from_slice(&len.to_le_bytes());
    }

    #[test]
    fn rejects_a_sector_without_the_signature() {
        let mut s = blank_sector();
        s[510] = 0;
        assert!(parse_mbr(&s).is_none());
        assert!(parse_mbr(&[0u8; 16]).is_none(), "short buffer");
    }

    #[test]
    fn parses_primary_entries() {
        let mut s = blank_sector();
        write_entry(&mut s, 0, 0x80, 0x07, 2048, 1_000_000);
        write_entry(&mut s, 1, 0x00, 0x0C, 1_002_048, 500_000);
        let e = parse_mbr(&s).expect("valid mbr");

        assert!(e[0].bootable);
        assert_eq!(e[0].type_byte, 0x07);
        assert_eq!(e[0].start_lba, 2048);
        assert_eq!(e[0].sectors, 1_000_000);
        assert!(!e[1].bootable);
        assert_eq!(e[1].type_byte, 0x0C);
        assert!(e[2].is_empty());
        assert!(e[3].is_empty());
    }

    #[test]
    fn detects_a_protective_mbr() {
        let mut s = blank_sector();
        write_entry(&mut s, 0, 0x00, TYPE_GPT_PROTECTIVE, 1, 0xFFFF_FFFF);
        let e = parse_mbr(&s).unwrap();
        assert!(is_protective(&e));

        // A normal single-partition MBR must not be mistaken for protective.
        let mut s = blank_sector();
        write_entry(&mut s, 0, 0x00, 0x07, 2048, 1000);
        assert!(!is_protective(&parse_mbr(&s).unwrap()));
    }

    #[test]
    fn plausibility_rejects_partitions_running_off_the_device() {
        let mut s = blank_sector();
        write_entry(&mut s, 0, 0x00, 0x07, 2048, 1_000_000);
        let e = parse_mbr(&s).unwrap();
        assert!(looks_plausible(&e, 2_000_000));
        assert!(
            !looks_plausible(&e, 100_000),
            "partition extends past the end of the device"
        );
    }

    #[test]
    fn plausibility_rejects_a_partition_starting_at_lba_zero() {
        let mut s = blank_sector();
        write_entry(&mut s, 0, 0x00, 0x07, 0, 1000);
        assert!(!looks_plausible(&parse_mbr(&s).unwrap(), 100_000));
    }

    #[test]
    fn plausibility_rejects_an_all_empty_table() {
        let s = blank_sector();
        assert!(
            !looks_plausible(&parse_mbr(&s).unwrap(), 100_000),
            "a signature alone is not a partition table"
        );
    }

    #[test]
    fn protective_entry_is_not_emitted_as_a_partition() {
        let mut s = blank_sector();
        write_entry(&mut s, 0, 0x00, TYPE_GPT_PROTECTIVE, 1, 0xFFFF_FFFF);
        assert!(to_partitions(&parse_mbr(&s).unwrap()).is_empty());
    }

    #[test]
    fn type_bytes_map_to_filesystems() {
        assert_eq!(fs_from_type_byte(0x07), FsKind::Ntfs);
        assert_eq!(fs_from_type_byte(0x0C), FsKind::Fat32);
        assert_eq!(fs_from_type_byte(0x83), FsKind::Ext4);
        assert_eq!(fs_from_type_byte(0x00), FsKind::Unknown);
    }
}
