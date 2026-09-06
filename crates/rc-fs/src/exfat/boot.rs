//! The exFAT boot sector.
//!
//! exFAT stores its geometry as base-2 logarithms rather than counts, which is
//! why the fields look unfamiliar next to FAT's BPB.

use crate::error::{FsError, Result};

#[derive(Clone, Copy, Debug)]
pub struct ExfatBoot {
    /// Sector offset of the volume from the start of the partition.
    pub partition_offset: u64,
    pub volume_length: u64,
    pub fat_offset_sectors: u32,
    pub fat_length_sectors: u32,
    pub cluster_heap_offset_sectors: u32,
    pub cluster_count: u32,
    pub first_cluster_of_root: u32,
    pub volume_serial: u32,
    pub bytes_per_sector_shift: u8,
    pub sectors_per_cluster_shift: u8,
    pub number_of_fats: u8,
    pub percent_in_use: u8,
}

impl ExfatBoot {
    pub fn bytes_per_sector(&self) -> u64 {
        1u64 << self.bytes_per_sector_shift
    }

    pub fn sectors_per_cluster(&self) -> u64 {
        1u64 << self.sectors_per_cluster_shift
    }

    pub fn cluster_bytes(&self) -> u64 {
        self.bytes_per_sector() << self.sectors_per_cluster_shift
    }

    pub fn fat_offset_bytes(&self) -> u64 {
        self.fat_offset_sectors as u64 * self.bytes_per_sector()
    }

    pub fn cluster_heap_offset_bytes(&self) -> u64 {
        self.cluster_heap_offset_sectors as u64 * self.bytes_per_sector()
    }

    /// Byte offset of a cluster within the volume. Clusters are numbered from
    /// 2, as in FAT.
    pub fn cluster_offset(&self, cluster: u32) -> Option<u64> {
        if cluster < 2 {
            return None;
        }
        Some(self.cluster_heap_offset_bytes() + (cluster as u64 - 2) * self.cluster_bytes())
    }
}

pub fn parse_boot(sector: &[u8]) -> Result<ExfatBoot> {
    if sector.len() < 512 {
        return Err(FsError::BadBootSector {
            detail: "boot sector shorter than 512 bytes".into(),
        });
    }
    if &sector[3..11] != b"EXFAT   " {
        return Err(FsError::BadBootSector {
            detail: format!(
                "OEM id is {:?}, not \"EXFAT   \"",
                String::from_utf8_lossy(&sector[3..11])
            ),
        });
    }
    // Bytes 11..64 must be zero in a valid exFAT boot sector; a FAT BPB here
    // means something has mislabelled the volume.
    if sector[11..64].iter().any(|&b| b != 0) {
        return Err(FsError::BadBootSector {
            detail: "the MustBeZero region contains a FAT BPB".into(),
        });
    }

    let bytes_per_sector_shift = sector[0x6C];
    let sectors_per_cluster_shift = sector[0x6D];
    if !(9..=12).contains(&bytes_per_sector_shift) {
        return Err(FsError::BadBootSector {
            detail: format!("BytesPerSectorShift {bytes_per_sector_shift} is out of range"),
        });
    }
    if sectors_per_cluster_shift > 25 - bytes_per_sector_shift {
        return Err(FsError::BadBootSector {
            detail: format!("SectorsPerClusterShift {sectors_per_cluster_shift} is out of range"),
        });
    }

    let boot = ExfatBoot {
        partition_offset: u64::from_le_bytes(sector[0x40..0x48].try_into().unwrap()),
        volume_length: u64::from_le_bytes(sector[0x48..0x50].try_into().unwrap()),
        fat_offset_sectors: u32::from_le_bytes(sector[0x50..0x54].try_into().unwrap()),
        fat_length_sectors: u32::from_le_bytes(sector[0x54..0x58].try_into().unwrap()),
        cluster_heap_offset_sectors: u32::from_le_bytes(sector[0x58..0x5C].try_into().unwrap()),
        cluster_count: u32::from_le_bytes(sector[0x5C..0x60].try_into().unwrap()),
        first_cluster_of_root: u32::from_le_bytes(sector[0x60..0x64].try_into().unwrap()),
        volume_serial: u32::from_le_bytes(sector[0x64..0x68].try_into().unwrap()),
        bytes_per_sector_shift,
        sectors_per_cluster_shift,
        number_of_fats: sector[0x6E],
        percent_in_use: sector[0x70],
    };

    if boot.volume_length == 0 || boot.cluster_count == 0 {
        return Err(FsError::BadBootSector {
            detail: "volume or cluster count is zero".into(),
        });
    }
    if boot.first_cluster_of_root < 2 {
        return Err(FsError::BadBootSector {
            detail: format!(
                "root directory cluster {} is below the first data cluster",
                boot.first_cluster_of_root
            ),
        });
    }

    Ok(boot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot_sector() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0] = 0xEB;
        b[1] = 0x76;
        b[2] = 0x90;
        b[3..11].copy_from_slice(b"EXFAT   ");
        b[0x48..0x50].copy_from_slice(&1_048_576u64.to_le_bytes());
        b[0x50..0x54].copy_from_slice(&2048u32.to_le_bytes());
        b[0x54..0x58].copy_from_slice(&512u32.to_le_bytes());
        b[0x58..0x5C].copy_from_slice(&4096u32.to_le_bytes());
        b[0x5C..0x60].copy_from_slice(&130_000u32.to_le_bytes());
        b[0x60..0x64].copy_from_slice(&5u32.to_le_bytes());
        b[0x6C] = 9; // 512 bytes/sector
        b[0x6D] = 3; // 8 sectors/cluster => 4 KiB
        b[0x6E] = 1;
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn parses_and_derives_geometry_from_shifts() {
        let e = parse_boot(&boot_sector()).expect("parses");
        assert_eq!(e.bytes_per_sector(), 512);
        assert_eq!(e.sectors_per_cluster(), 8);
        assert_eq!(e.cluster_bytes(), 4096);
        assert_eq!(e.first_cluster_of_root, 5);
        assert_eq!(e.fat_offset_bytes(), 2048 * 512);
        assert_eq!(e.cluster_heap_offset_bytes(), 4096 * 512);
    }

    #[test]
    fn cluster_offsets_are_relative_to_the_heap_and_start_at_two() {
        let e = parse_boot(&boot_sector()).unwrap();
        assert_eq!(e.cluster_offset(1), None);
        assert_eq!(e.cluster_offset(2), Some(4096 * 512));
        assert_eq!(e.cluster_offset(3), Some(4096 * 512 + 4096));
    }

    #[test]
    fn rejects_a_fat_volume() {
        let mut b = boot_sector();
        b[3..11].copy_from_slice(b"MSDOS5.0");
        assert!(parse_boot(&b).is_err());
    }

    #[test]
    fn rejects_a_bpb_in_the_must_be_zero_region() {
        let mut b = boot_sector();
        b[11] = 0x02; // a FAT BPB's bytes-per-sector field
        let e = parse_boot(&b).unwrap_err();
        assert!(format!("{e}").contains("MustBeZero"));
    }

    #[test]
    fn rejects_out_of_range_shifts() {
        let mut b = boot_sector();
        b[0x6C] = 20;
        assert!(parse_boot(&b).is_err());

        let mut b = boot_sector();
        b[0x6D] = 30;
        assert!(parse_boot(&b).is_err());
    }

    #[test]
    fn rejects_a_root_cluster_below_two() {
        let mut b = boot_sector();
        b[0x60..0x64].copy_from_slice(&1u32.to_le_bytes());
        assert!(parse_boot(&b).is_err());
    }
}
