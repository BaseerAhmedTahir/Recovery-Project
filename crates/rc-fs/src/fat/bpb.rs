//! The FAT BIOS Parameter Block and the cluster arithmetic derived from it.

use crate::error::{FsError, Result};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FatKind {
    Fat12,
    Fat16,
    Fat32,
}

#[derive(Clone, Copy, Debug)]
pub struct Bpb {
    pub kind: FatKind,
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub reserved_sectors: u32,
    pub num_fats: u32,
    pub root_entries: u32,
    pub total_sectors: u64,
    pub fat_size_sectors: u32,
    /// FAT32 only: the cluster the root directory starts at.
    pub root_cluster: u32,
    pub cluster_count: u64,
}

impl Bpb {
    pub fn cluster_bytes(&self) -> u64 {
        self.bytes_per_sector as u64 * self.sectors_per_cluster as u64
    }

    /// Sectors occupied by a FAT12/16 fixed-size root directory. Zero on FAT32.
    pub fn root_dir_sectors(&self) -> u32 {
        (self.root_entries * 32).div_ceil(self.bytes_per_sector)
    }

    pub fn first_data_sector(&self) -> u64 {
        self.reserved_sectors as u64
            + (self.num_fats as u64 * self.fat_size_sectors as u64)
            + self.root_dir_sectors() as u64
    }

    /// Byte offset of a data cluster within the volume.
    ///
    /// Data clusters are numbered from 2; 0 and 1 are reserved markers in the
    /// FAT and have no storage.
    pub fn cluster_offset(&self, cluster: u32) -> Option<u64> {
        if cluster < 2 {
            return None;
        }
        let sector =
            self.first_data_sector() + (cluster as u64 - 2) * self.sectors_per_cluster as u64;
        Some(sector * self.bytes_per_sector as u64)
    }

    /// Byte offset of the FAT12/16 root directory region.
    pub fn root_dir_offset(&self) -> u64 {
        (self.reserved_sectors as u64 + self.num_fats as u64 * self.fat_size_sectors as u64)
            * self.bytes_per_sector as u64
    }

    pub fn fat_offset(&self, which: u32) -> u64 {
        (self.reserved_sectors as u64 + which as u64 * self.fat_size_sectors as u64)
            * self.bytes_per_sector as u64
    }

    /// Highest valid data cluster number.
    pub fn max_cluster(&self) -> u32 {
        (self.cluster_count + 1) as u32
    }
}

pub fn parse_bpb(sector: &[u8]) -> Result<Bpb> {
    if sector.len() < 512 {
        return Err(FsError::BadBootSector {
            detail: "boot sector shorter than 512 bytes".into(),
        });
    }
    // exFAT reuses the jump instruction but is a completely different format;
    // catching it here gives a better message than failing on the BPB.
    if &sector[3..11] == b"EXFAT   " {
        return Err(FsError::BadBootSector {
            detail: "this is an exFAT volume, not FAT12/16/32".into(),
        });
    }

    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]) as u32;
    let sectors_per_cluster = sector[13] as u32;
    let reserved_sectors = u16::from_le_bytes([sector[14], sector[15]]) as u32;
    let num_fats = sector[16] as u32;
    let root_entries = u16::from_le_bytes([sector[17], sector[18]]) as u32;
    let total_16 = u16::from_le_bytes([sector[19], sector[20]]) as u64;
    let fat_size_16 = u16::from_le_bytes([sector[22], sector[23]]) as u32;
    let total_32 = u32::from_le_bytes(sector[32..36].try_into().unwrap()) as u64;
    let fat_size_32 = u32::from_le_bytes(sector[36..40].try_into().unwrap());
    let root_cluster = u32::from_le_bytes(sector[44..48].try_into().unwrap());

    if !bytes_per_sector.is_power_of_two() || !(512..=4096).contains(&bytes_per_sector) {
        return Err(FsError::BadBootSector {
            detail: format!("implausible sector size {bytes_per_sector}"),
        });
    }
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
        return Err(FsError::BadBootSector {
            detail: format!("implausible cluster size {sectors_per_cluster} sectors"),
        });
    }
    if reserved_sectors == 0 || !(1..=2).contains(&num_fats) {
        return Err(FsError::BadBootSector {
            detail: format!("{reserved_sectors} reserved sectors, {num_fats} FATs"),
        });
    }

    let total_sectors = if total_16 != 0 { total_16 } else { total_32 };
    let fat_size_sectors = if fat_size_16 != 0 {
        fat_size_16
    } else {
        fat_size_32
    };
    if total_sectors == 0 || fat_size_sectors == 0 {
        return Err(FsError::BadBootSector {
            detail: "volume or FAT declares zero size".into(),
        });
    }

    let root_dir_sectors = (root_entries * 32).div_ceil(bytes_per_sector) as u64;
    let data_sectors = total_sectors
        .checked_sub(
            reserved_sectors as u64 + num_fats as u64 * fat_size_sectors as u64 + root_dir_sectors,
        )
        .ok_or_else(|| FsError::BadBootSector {
            detail: "metadata regions are larger than the volume".into(),
        })?;
    let cluster_count = data_sectors / sectors_per_cluster as u64;

    // Microsoft's rule: the cluster count alone decides the FAT width. The
    // "FAT32   " string in the boot sector is documentation, not authority.
    let kind = if cluster_count < 4085 {
        FatKind::Fat12
    } else if cluster_count < 65525 {
        FatKind::Fat16
    } else {
        FatKind::Fat32
    };

    Ok(Bpb {
        kind,
        bytes_per_sector,
        sectors_per_cluster,
        reserved_sectors,
        num_fats,
        root_entries,
        total_sectors,
        fat_size_sectors,
        root_cluster: if kind == FatKind::Fat32 {
            root_cluster
        } else {
            0
        },
        cluster_count,
    })
}

/// Read one FAT entry.
pub fn fat_entry(fat: &[u8], kind: FatKind, cluster: u32) -> Option<u32> {
    match kind {
        FatKind::Fat32 => {
            let o = cluster as usize * 4;
            if o + 4 > fat.len() {
                return None;
            }
            // The top four bits are reserved and must be masked off.
            Some(u32::from_le_bytes(fat[o..o + 4].try_into().ok()?) & 0x0FFF_FFFF)
        }
        FatKind::Fat16 => {
            let o = cluster as usize * 2;
            if o + 2 > fat.len() {
                return None;
            }
            Some(u16::from_le_bytes([fat[o], fat[o + 1]]) as u32)
        }
        FatKind::Fat12 => {
            let o = cluster as usize + (cluster as usize / 2);
            if o + 2 > fat.len() {
                return None;
            }
            let raw = u16::from_le_bytes([fat[o], fat[o + 1]]);
            Some(if cluster & 1 == 1 {
                (raw >> 4) as u32
            } else {
                (raw & 0x0FFF) as u32
            })
        }
    }
}

/// Is this FAT entry an end-of-chain marker?
pub fn is_end_of_chain(kind: FatKind, value: u32) -> bool {
    match kind {
        FatKind::Fat12 => value >= 0x0FF8,
        FatKind::Fat16 => value >= 0xFFF8,
        FatKind::Fat32 => value >= 0x0FFF_FFF8,
    }
}

/// Follow a cluster chain, bounded so a corrupt FAT cannot loop forever.
pub fn follow_chain(fat: &[u8], bpb: &Bpb, start: u32, max: usize) -> Vec<u32> {
    let mut out = Vec::new();
    let mut c = start;
    let mut seen = std::collections::HashSet::new();
    while c >= 2 && c <= bpb.max_cluster() && out.len() < max {
        if !seen.insert(c) {
            break; // a loop in the chain
        }
        out.push(c);
        match fat_entry(fat, bpb.kind, c) {
            Some(next) if !is_end_of_chain(bpb.kind, next) && next >= 2 => c = next,
            _ => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fat32_boot() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0] = 0xEB;
        b[1] = 0x58;
        b[2] = 0x90;
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = 8;
        b[14..16].copy_from_slice(&32u16.to_le_bytes());
        b[16] = 2;
        b[32..36].copy_from_slice(&1_048_576u32.to_le_bytes());
        b[36..40].copy_from_slice(&1024u32.to_le_bytes());
        b[44..48].copy_from_slice(&2u32.to_le_bytes());
        b[82..90].copy_from_slice(b"FAT32   ");
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn parses_a_fat32_bpb() {
        let p = parse_bpb(&fat32_boot()).expect("parses");
        assert_eq!(p.kind, FatKind::Fat32);
        assert_eq!(p.cluster_bytes(), 4096);
        assert_eq!(p.root_cluster, 2);
        assert_eq!(p.root_dir_sectors(), 0, "FAT32 has no fixed root region");
        assert_eq!(p.first_data_sector(), 32 + 2 * 1024);
    }

    #[test]
    fn cluster_offsets_start_at_cluster_two() {
        let p = parse_bpb(&fat32_boot()).unwrap();
        assert_eq!(p.cluster_offset(0), None, "clusters 0 and 1 are markers");
        assert_eq!(p.cluster_offset(1), None);
        assert_eq!(
            p.cluster_offset(2),
            Some(p.first_data_sector() * 512),
            "cluster 2 is the first data cluster"
        );
        assert_eq!(p.cluster_offset(3), Some((p.first_data_sector() + 8) * 512));
    }

    #[test]
    fn rejects_an_exfat_volume_with_a_clear_message() {
        let mut b = fat32_boot();
        b[3..11].copy_from_slice(b"EXFAT   ");
        let e = parse_bpb(&b).unwrap_err();
        assert!(format!("{e}").contains("exFAT"));
    }

    #[test]
    fn fat_width_is_decided_by_cluster_count_not_the_label() {
        // Small volume labelled FAT32 is really FAT16.
        let mut b = fat32_boot();
        b[32..36].copy_from_slice(&40_960u32.to_le_bytes());
        b[36..40].copy_from_slice(&40u32.to_le_bytes());
        let p = parse_bpb(&b).unwrap();
        assert_eq!(p.kind, FatKind::Fat16);
    }

    #[test]
    fn reads_fat32_entries_masking_reserved_bits() {
        let mut fat = vec![0u8; 64];
        fat[8..12].copy_from_slice(&0xF000_0005u32.to_le_bytes());
        assert_eq!(
            fat_entry(&fat, FatKind::Fat32, 2),
            Some(5),
            "the top four bits are reserved"
        );
    }

    #[test]
    fn reads_fat12_packed_entries() {
        // Clusters 2 and 3 packed into three bytes: 0x123, 0x456.
        let fat = vec![0, 0, 0, 0x23, 0x61, 0x45];
        assert_eq!(fat_entry(&fat, FatKind::Fat12, 2), Some(0x123));
        assert_eq!(fat_entry(&fat, FatKind::Fat12, 3), Some(0x456));
    }

    #[test]
    fn follows_a_chain_and_stops_at_the_end_marker() {
        let bpb = parse_bpb(&fat32_boot()).unwrap();
        let mut fat = vec![0u8; 4096];
        // 2 -> 3 -> 4 -> EOC
        fat[8..12].copy_from_slice(&3u32.to_le_bytes());
        fat[12..16].copy_from_slice(&4u32.to_le_bytes());
        fat[16..20].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        assert_eq!(follow_chain(&fat, &bpb, 2, 100), vec![2, 3, 4]);
    }

    #[test]
    fn a_looping_chain_terminates() {
        let bpb = parse_bpb(&fat32_boot()).unwrap();
        let mut fat = vec![0u8; 4096];
        // 2 -> 3 -> 2 -> ...
        fat[8..12].copy_from_slice(&3u32.to_le_bytes());
        fat[12..16].copy_from_slice(&2u32.to_le_bytes());
        let chain = follow_chain(&fat, &bpb, 2, 1000);
        assert_eq!(chain, vec![2, 3], "a cycle must not hang the scan");
    }
}
