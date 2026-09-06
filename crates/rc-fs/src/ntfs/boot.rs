//! The NTFS boot sector / BPB.

use crate::error::{FsError, Result};

#[derive(Clone, Copy, Debug)]
pub struct NtfsBoot {
    pub bytes_per_sector: u32,
    pub sectors_per_cluster: u32,
    pub total_sectors: u64,
    /// Cluster number of the $MFT.
    pub mft_lcn: u64,
    pub mft_mirror_lcn: u64,
    /// Size of one MFT record in bytes.
    pub mft_record_bytes: u32,
    pub index_record_bytes: u32,
    pub volume_serial: u64,
}

impl NtfsBoot {
    pub fn cluster_bytes(&self) -> u64 {
        self.bytes_per_sector as u64 * self.sectors_per_cluster as u64
    }

    pub fn mft_offset(&self) -> u64 {
        self.mft_lcn * self.cluster_bytes()
    }

    pub fn total_clusters(&self) -> u64 {
        if self.sectors_per_cluster == 0 {
            0
        } else {
            self.total_sectors / self.sectors_per_cluster as u64
        }
    }
}

/// NTFS encodes the cluster and record sizes with a shared convention: a
/// positive value is a count, while a value with the high bit set is the
/// negative base-2 logarithm of a byte size. A record size of 0xF6 means
/// 2^(256-246) = 1024 bytes, which is what almost every volume uses.
fn decode_sized_field(raw: i8, unit_bytes: u64) -> u64 {
    if raw > 0 {
        raw as u64 * unit_bytes
    } else if raw == 0 {
        // Zero is not a valid encoding in either form. Returning 0 lets the
        // caller reject it; the obvious `1 << -raw` would turn it into 1 and
        // silently accept a corrupt boot sector.
        0
    } else {
        let shift = (-(raw as i32)) as u32;
        if shift >= 64 {
            0
        } else {
            1u64 << shift
        }
    }
}

pub fn parse_boot(sector: &[u8]) -> Result<NtfsBoot> {
    if sector.len() < 512 {
        return Err(FsError::BadBootSector {
            detail: "boot sector is shorter than 512 bytes".into(),
        });
    }
    if &sector[3..11] != b"NTFS    " {
        return Err(FsError::BadBootSector {
            detail: format!(
                "OEM id is {:?}, not \"NTFS    \"",
                String::from_utf8_lossy(&sector[3..11])
            ),
        });
    }

    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]) as u32;
    if !bytes_per_sector.is_power_of_two() || !(256..=4096).contains(&bytes_per_sector) {
        return Err(FsError::BadBootSector {
            detail: format!("implausible sector size {bytes_per_sector}"),
        });
    }
    let sectors_per_cluster = decode_sized_field(sector[13] as i8, 1) as u32;
    if sectors_per_cluster == 0 || sectors_per_cluster > 65536 {
        return Err(FsError::BadBootSector {
            detail: format!("implausible cluster size {sectors_per_cluster} sectors"),
        });
    }

    let total_sectors = u64::from_le_bytes(sector[0x28..0x30].try_into().unwrap());
    let mft_lcn = u64::from_le_bytes(sector[0x30..0x38].try_into().unwrap());
    let mft_mirror_lcn = u64::from_le_bytes(sector[0x38..0x40].try_into().unwrap());
    let cluster_bytes = bytes_per_sector as u64 * sectors_per_cluster as u64;

    let mft_record_bytes = decode_sized_field(sector[0x40] as i8, cluster_bytes) as u32;
    let index_record_bytes = decode_sized_field(sector[0x44] as i8, cluster_bytes) as u32;

    if mft_record_bytes == 0 || mft_record_bytes > 65536 || !mft_record_bytes.is_power_of_two() {
        return Err(FsError::BadBootSector {
            detail: format!("implausible MFT record size {mft_record_bytes}"),
        });
    }
    if total_sectors == 0 {
        return Err(FsError::BadBootSector {
            detail: "volume declares zero sectors".into(),
        });
    }

    Ok(NtfsBoot {
        bytes_per_sector,
        sectors_per_cluster,
        total_sectors,
        mft_lcn,
        mft_mirror_lcn,
        mft_record_bytes,
        index_record_bytes,
        volume_serial: u64::from_le_bytes(sector[0x48..0x50].try_into().unwrap()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boot(spc: u8, rec: i8) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = spc;
        b[0x28..0x30].copy_from_slice(&1_048_575u64.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        b[0x40] = rec as u8;
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn parses_a_normal_boot_sector() {
        let b = boot(8, 0xF6u8 as i8);
        let n = parse_boot(&b).expect("parses");
        assert_eq!(n.bytes_per_sector, 512);
        assert_eq!(n.sectors_per_cluster, 8);
        assert_eq!(n.cluster_bytes(), 4096);
        assert_eq!(n.mft_lcn, 4);
        assert_eq!(n.mft_offset(), 4 * 4096);
        assert_eq!(
            n.mft_record_bytes, 1024,
            "0xF6 is -10, meaning 2^10 = 1024 bytes"
        );
    }

    #[test]
    fn decodes_a_positive_record_size_as_a_cluster_count() {
        let b = boot(8, 2);
        let n = parse_boot(&b).unwrap();
        assert_eq!(n.mft_record_bytes, 2 * 4096, "positive means clusters");
    }

    #[test]
    fn rejects_a_non_ntfs_sector() {
        let mut b = boot(8, 0xF6u8 as i8);
        b[3..11].copy_from_slice(b"EXFAT   ");
        assert!(parse_boot(&b).is_err());
    }

    #[test]
    fn rejects_implausible_geometry() {
        let mut b = boot(8, 0xF6u8 as i8);
        b[11..13].copy_from_slice(&777u16.to_le_bytes());
        assert!(parse_boot(&b).is_err());

        let mut b = boot(0, 0xF6u8 as i8);
        b[13] = 0;
        assert!(parse_boot(&b).is_err());
    }

    #[test]
    fn rejects_a_zero_length_volume() {
        let mut b = boot(8, 0xF6u8 as i8);
        b[0x28..0x30].copy_from_slice(&0u64.to_le_bytes());
        assert!(parse_boot(&b).is_err());
    }
}
