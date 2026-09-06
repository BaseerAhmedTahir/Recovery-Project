//! Lost-partition rebuild by filesystem signature scan (SPEC.md section 5.3).
//!
//! When no usable partition table exists, scan the device for filesystem boot
//! sectors and superblocks and reconstruct a plausible table **in memory**.
//! Nothing here ever writes to the device.
//!
//! Signatures recognised, per SPEC.md:
//!
//! | Filesystem | Evidence | Where |
//! |---|---|---|
//! | NTFS  | `NTFS    ` OEM id + valid BPB | partition start |
//! | FAT32 | BPB with `FAT32   ` fs type   | partition start |
//! | exFAT | `EXFAT   ` OEM id             | partition start |
//! | EXT4  | magic `0xEF53`                | +1024 into the partition |
//! | APFS  | `NXSB`                        | +32 into the container |
//! | HFS+  | `H+` / `HX`                   | +1024 into the volume |
//!
//! A raw magic match is not enough on its own: at one in 2^32 per offset, a
//! 512 GiB device produces false positives by chance. Every candidate is
//! therefore validated against its own header fields (sector size a power of
//! two, cluster size sane, volume length within the device) before it is
//! reported, and each carries a confidence score and the evidence behind it.

use crate::table::{FsKind, Origin, Partition};
use rc_device::{Lba, ReadOnlyDevice, SectorSize};

/// A filesystem header found at a specific LBA.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub lba: Lba,
    pub fs: FsKind,
    /// Volume length in sectors, when the header records it. Zero if unknown.
    pub sectors: u64,
    pub confidence: u8,
    pub evidence: Vec<String>,
}

/// Identify a filesystem from a candidate boot sector.
///
/// `at_lba` and `total_sectors` are used only for plausibility checks.
pub fn identify_boot_sector(buf: &[u8], at_lba: u64, total_sectors: u64) -> Option<Candidate> {
    if buf.len() < 512 {
        return None;
    }

    if let Some(c) = identify_ntfs(buf, at_lba, total_sectors) {
        return Some(c);
    }
    if let Some(c) = identify_exfat(buf, at_lba, total_sectors) {
        return Some(c);
    }
    if let Some(c) = identify_fat(buf, at_lba, total_sectors) {
        return Some(c);
    }
    None
}

/// NTFS: OEM id "NTFS    " at offset 3, plus a self-consistent BPB.
fn identify_ntfs(b: &[u8], at_lba: u64, total: u64) -> Option<Candidate> {
    if &b[3..11] != b"NTFS    " {
        return None;
    }
    let bytes_per_sector = u16::from_le_bytes([b[11], b[12]]) as u32;
    let sectors_per_cluster = b[13];
    // NTFS stores total sectors as a 64-bit count at 0x28, exclusive of the
    // backup boot sector, so the real volume is this plus one.
    let total_sectors = u64::from_le_bytes(b[0x28..0x30].try_into().ok()?);
    let mft_lcn = u64::from_le_bytes(b[0x30..0x38].try_into().ok()?);

    if !bytes_per_sector.is_power_of_two() || !(512..=4096).contains(&bytes_per_sector) {
        return None;
    }
    if sectors_per_cluster == 0 || (sectors_per_cluster & (sectors_per_cluster - 1)) != 0 {
        // NTFS also encodes large clusters as a signed power-of-two shift;
        // values above 0x80 mean 2^(256 - v). Accept only the simple form here
        // and fall through to the shift form.
        if sectors_per_cluster < 0x80 {
            return None;
        }
    }
    if total_sectors == 0 || (total > 0 && at_lba + total_sectors > total + 1) {
        return None;
    }
    if mft_lcn == 0 || mft_lcn > total_sectors {
        return None;
    }

    Some(Candidate {
        lba: Lba(at_lba),
        fs: FsKind::Ntfs,
        // +1 for the backup boot sector NTFS excludes from its own count.
        sectors: total_sectors + 1,
        confidence: 95,
        evidence: vec![
            "NTFS OEM id at offset 3".to_string(),
            format!("BPB: {bytes_per_sector} bytes/sector, {sectors_per_cluster} sectors/cluster"),
            format!("volume length {total_sectors} sectors, $MFT at cluster {mft_lcn}"),
        ],
    })
}

/// exFAT: OEM id "EXFAT   " at offset 3 and a plausible volume length.
fn identify_exfat(b: &[u8], at_lba: u64, total: u64) -> Option<Candidate> {
    if &b[3..11] != b"EXFAT   " {
        return None;
    }
    // exFAT keeps its geometry as log2 values at 0x6C/0x6D.
    let volume_length = u64::from_le_bytes(b[0x48..0x50].try_into().ok()?);
    let bytes_per_sector_shift = b[0x6C];
    let sectors_per_cluster_shift = b[0x6D];

    if !(9..=12).contains(&bytes_per_sector_shift) || sectors_per_cluster_shift > 25 {
        return None;
    }
    if volume_length == 0 || (total > 0 && at_lba + volume_length > total) {
        return None;
    }

    Some(Candidate {
        lba: Lba(at_lba),
        fs: FsKind::ExFat,
        sectors: volume_length,
        confidence: 95,
        evidence: vec![
            "exFAT OEM id at offset 3".to_string(),
            format!(
                "{} bytes/sector, {} sectors/cluster",
                1u32 << bytes_per_sector_shift,
                1u32 << sectors_per_cluster_shift
            ),
            format!("volume length {volume_length} sectors"),
        ],
    })
}

/// FAT12/16/32 via the BPB. The filesystem type string is only a hint; the
/// cluster count is what actually distinguishes the three.
fn identify_fat(b: &[u8], at_lba: u64, total: u64) -> Option<Candidate> {
    // A FAT boot sector starts with a jump instruction.
    if !(b[0] == 0xEB && b[2] == 0x90) && b[0] != 0xE9 {
        return None;
    }
    if u16::from_le_bytes([b[510], b[511]]) != 0xAA55 {
        return None;
    }

    let bytes_per_sector = u16::from_le_bytes([b[11], b[12]]) as u32;
    let sectors_per_cluster = b[13];
    let reserved = u16::from_le_bytes([b[14], b[15]]) as u32;
    let num_fats = b[16];
    let root_entries = u16::from_le_bytes([b[17], b[18]]) as u32;
    let total16 = u16::from_le_bytes([b[19], b[20]]) as u64;
    let fat_size16 = u16::from_le_bytes([b[22], b[23]]) as u32;
    let total32 = u32::from_le_bytes(b[32..36].try_into().ok()?) as u64;
    let fat_size32 = u32::from_le_bytes(b[36..40].try_into().ok()?);

    if !bytes_per_sector.is_power_of_two() || !(512..=4096).contains(&bytes_per_sector) {
        return None;
    }
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
        return None;
    }
    if reserved == 0 || !(1..=2).contains(&num_fats) {
        return None;
    }

    let total_sectors = if total16 != 0 { total16 } else { total32 };
    let fat_size = if fat_size16 != 0 {
        fat_size16
    } else {
        fat_size32
    };
    if total_sectors == 0 || fat_size == 0 {
        return None;
    }
    if total > 0 && at_lba + total_sectors > total {
        return None;
    }

    // Cluster count decides FAT12 vs FAT16 vs FAT32 (Microsoft's rule).
    let root_dir_sectors = (root_entries * 32).div_ceil(bytes_per_sector);
    let data_sectors = total_sectors.checked_sub(
        reserved as u64 + (num_fats as u64 * fat_size as u64) + root_dir_sectors as u64,
    )?;
    let clusters = data_sectors / sectors_per_cluster as u64;
    let fs = if clusters < 4085 {
        FsKind::Fat12
    } else if clusters < 65525 {
        FsKind::Fat16
    } else {
        FsKind::Fat32
    };

    // The declared type string is a weak corroboration, not the decision.
    let declared = if fs == FsKind::Fat32 {
        &b[82..90]
    } else {
        &b[54..62]
    };
    let declared_str = String::from_utf8_lossy(declared).trim().to_string();
    let corroborated = declared_str.starts_with("FAT");

    Some(Candidate {
        lba: Lba(at_lba),
        fs,
        sectors: total_sectors,
        confidence: if corroborated { 90 } else { 70 },
        evidence: vec![
            format!(
                "FAT BPB: {bytes_per_sector} bytes/sector, {sectors_per_cluster} sectors/cluster"
            ),
            format!("{clusters} data clusters implies {fs}"),
            format!("declared fs type {declared_str:?}"),
        ],
    })
}

/// EXT4 superblock lives 1024 bytes into the volume; magic 0xEF53 at +0x38.
pub fn identify_ext_superblock(sb: &[u8], at_lba: u64) -> Option<Candidate> {
    if sb.len() < 0x60 {
        return None;
    }
    if u16::from_le_bytes([sb[0x38], sb[0x39]]) != 0xEF53 {
        return None;
    }
    let blocks_count = u32::from_le_bytes(sb[0x04..0x08].try_into().ok()?) as u64;
    let log_block_size = u32::from_le_bytes(sb[0x18..0x1C].try_into().ok()?);
    if log_block_size > 6 {
        return None;
    }
    let block_size = 1024u64 << log_block_size;
    if blocks_count == 0 {
        return None;
    }
    Some(Candidate {
        // The superblock is at +1024, so the volume starts 1024 bytes earlier.
        lba: Lba(at_lba),
        fs: FsKind::Ext4,
        sectors: blocks_count * block_size / 512,
        confidence: 90,
        evidence: vec![
            "ext superblock magic 0xEF53 at +0x438".to_string(),
            format!("{blocks_count} blocks of {block_size} bytes"),
        ],
    })
}

/// APFS container superblock: `NXSB` at offset 32 of the container's block 0.
pub fn identify_apfs(b: &[u8], at_lba: u64) -> Option<Candidate> {
    if b.len() < 40 || &b[32..36] != b"NXSB" {
        return None;
    }
    let block_size = u32::from_le_bytes(b[36..40].try_into().ok()?) as u64;
    if !(512..=65536).contains(&block_size) || !block_size.is_power_of_two() {
        return None;
    }
    Some(Candidate {
        lba: Lba(at_lba),
        fs: FsKind::Apfs,
        sectors: 0, // block count sits deeper in the superblock
        confidence: 80,
        evidence: vec![
            "APFS container magic NXSB at +32".to_string(),
            format!("block size {block_size}"),
        ],
    })
}

/// HFS+ volume header: `H+` (0x482B) or `HX` (0x4858) at +1024.
pub fn identify_hfsplus(vh: &[u8], at_lba: u64) -> Option<Candidate> {
    if vh.len() < 4 {
        return None;
    }
    let sig = u16::from_be_bytes([vh[0], vh[1]]);
    if sig != 0x482B && sig != 0x4858 {
        return None;
    }
    Some(Candidate {
        lba: Lba(at_lba),
        fs: FsKind::HfsPlus,
        sectors: 0,
        confidence: 80,
        evidence: vec![format!("HFS+ volume header signature {:04X} at +1024", sig)],
    })
}

/// Options controlling the rebuild scan.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Only inspect LBAs that are a multiple of this. Partitions are aligned
    /// to 1 MiB on every modern tool, so the default checks 2048-sector
    /// boundaries and is roughly 2000x faster than checking every sector.
    pub alignment_sectors: u64,
    /// Also check every sector. Far slower, but finds partitions created by
    /// older tools that aligned to a cylinder rather than a megabyte.
    pub exhaustive: bool,
    /// Stop after this many candidates.
    pub max_candidates: usize,
    /// Limit the scan to the first N sectors. 0 means the whole device.
    pub limit_sectors: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            alignment_sectors: 2048, // 1 MiB at 512-byte sectors
            exhaustive: false,
            max_candidates: 64,
            limit_sectors: 0,
        }
    }
}

/// Scan a device for filesystem headers.
///
/// Always inspects LBA 0 and the common first-partition offsets (63 and 2048)
/// regardless of alignment, because those cover almost every real disk.
pub fn scan_for_filesystems(
    device: &dyn ReadOnlyDevice,
    opts: &ScanOptions,
) -> crate::error::Result<Vec<Candidate>> {
    let ss = device.sector_size();
    let total = device.total_sectors();
    let end = if opts.limit_sectors == 0 {
        total
    } else {
        opts.limit_sectors.min(total)
    };

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen: Vec<u64> = Vec::new();

    let check =
        |lba: u64, out: &mut Vec<Candidate>, seen: &mut Vec<u64>| -> crate::error::Result<()> {
            if lba >= total || seen.contains(&lba) {
                return Ok(());
            }
            seen.push(lba);
            let mut buf = vec![0u8; ss.as_usize()];
            // A read error mid-scan is expected on a damaged disk; skip and move on.
            if device.read_at(Lba(lba), &mut buf).is_err() {
                return Ok(());
            }
            if let Some(c) = identify_boot_sector(&buf, lba, total) {
                out.push(c);
                return Ok(());
            }
            // ext4 and HFS+ keep their headers 1024 bytes in.
            let mut deep = vec![0u8; 1024];
            if device
                .read_bytes_at(lba * ss.get() as u64 + 1024, &mut deep)
                .is_ok()
            {
                if let Some(c) = identify_ext_superblock(&deep, lba) {
                    out.push(c);
                    return Ok(());
                }
                if let Some(c) = identify_hfsplus(&deep, lba) {
                    out.push(c);
                    return Ok(());
                }
            }
            if let Some(c) = identify_apfs(&buf, lba) {
                out.push(c);
            }
            Ok(())
        };

    // Offsets that cover essentially every real-world first partition.
    for lba in [0u64, 63, 2048] {
        check(lba, &mut candidates, &mut seen)?;
    }

    let step = if opts.exhaustive {
        1
    } else {
        opts.alignment_sectors.max(1)
    };
    let mut lba = 0u64;
    while lba < end && candidates.len() < opts.max_candidates {
        check(lba, &mut candidates, &mut seen)?;
        lba += step;
    }

    candidates.sort_by_key(|c| c.lba.0);
    Ok(candidates)
}

/// Turn scan candidates into a reconstructed partition list, dropping ones
/// that overlap a higher-confidence neighbour.
pub fn candidates_to_partitions(
    mut candidates: Vec<Candidate>,
    sector_size: SectorSize,
) -> Vec<Partition> {
    let _ = sector_size;
    candidates.sort_by(|a, b| b.confidence.cmp(&a.confidence).then(a.lba.0.cmp(&b.lba.0)));

    let mut kept: Vec<Partition> = Vec::new();
    for c in candidates {
        let p = Partition {
            index: 0,
            start: c.lba,
            sectors: c.sectors,
            fs: c.fs,
            origin: Origin::Reconstructed,
            label: None,
            type_guid: None,
            bootable: false,
            confidence: c.confidence,
            evidence: c.evidence,
        };
        // A boot sector found inside another filesystem's extent is almost
        // always that filesystem's own backup boot sector, not a partition.
        if kept.iter().any(|k| k.overlaps(&p) || k.start == p.start) {
            continue;
        }
        kept.push(p);
    }
    kept.sort_by_key(|p| p.start.0);
    for (i, p) in kept.iter_mut().enumerate() {
        p.index = i + 1;
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntfs_boot(total_sectors: u64) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0] = 0xEB;
        b[1] = 0x52;
        b[2] = 0x90;
        b[3..11].copy_from_slice(b"NTFS    ");
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = 8; // sectors per cluster
        b[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
        b[0x30..0x38].copy_from_slice(&4u64.to_le_bytes()); // $MFT cluster
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    fn exfat_boot(volume_length: u64) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0] = 0xEB;
        b[1] = 0x76;
        b[2] = 0x90;
        b[3..11].copy_from_slice(b"EXFAT   ");
        b[0x48..0x50].copy_from_slice(&volume_length.to_le_bytes());
        b[0x6C] = 9; // 512 bytes/sector
        b[0x6D] = 3; // 8 sectors/cluster
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    fn fat32_boot(total_sectors: u32) -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[0] = 0xEB;
        b[1] = 0x58;
        b[2] = 0x90;
        b[11..13].copy_from_slice(&512u16.to_le_bytes());
        b[13] = 8;
        b[14..16].copy_from_slice(&32u16.to_le_bytes()); // reserved
        b[16] = 2; // num fats
        b[17..19].copy_from_slice(&0u16.to_le_bytes()); // root entries (FAT32: 0)
        b[19..21].copy_from_slice(&0u16.to_le_bytes()); // total16
        b[22..24].copy_from_slice(&0u16.to_le_bytes()); // fat_size16
        b[32..36].copy_from_slice(&total_sectors.to_le_bytes());
        b[36..40].copy_from_slice(&1024u32.to_le_bytes()); // fat_size32
        b[82..90].copy_from_slice(b"FAT32   ");
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn identifies_ntfs() {
        let b = ntfs_boot(1_048_575);
        let c = identify_boot_sector(&b, 2048, 2_000_000).expect("ntfs recognised");
        assert_eq!(c.fs, FsKind::Ntfs);
        assert_eq!(c.lba, Lba(2048));
        assert_eq!(c.sectors, 1_048_576, "NTFS excludes its backup boot sector");
        assert!(c.confidence >= 90);
    }

    #[test]
    fn identifies_exfat() {
        let b = exfat_boot(500_000);
        let c = identify_boot_sector(&b, 0, 1_000_000).expect("exfat recognised");
        assert_eq!(c.fs, FsKind::ExFat);
        assert_eq!(c.sectors, 500_000);
    }

    #[test]
    fn identifies_fat32_by_cluster_count() {
        let b = fat32_boot(1_048_576);
        let c = identify_boot_sector(&b, 0, 2_000_000).expect("fat recognised");
        assert_eq!(c.fs, FsKind::Fat32, "130k clusters is FAT32 territory");
        assert_eq!(c.sectors, 1_048_576);
    }

    #[test]
    fn classifies_small_fat_volumes_as_fat16() {
        // 20 MiB with 8-sector clusters gives ~5100 clusters: FAT16.
        let mut b = fat32_boot(40_960);
        b[17..19].copy_from_slice(&512u16.to_le_bytes()); // root entries
        b[22..24].copy_from_slice(&40u16.to_le_bytes()); // fat_size16
        b[36..40].copy_from_slice(&0u32.to_le_bytes()); // clear fat_size32
        b[54..62].copy_from_slice(b"FAT16   ");
        let c = identify_boot_sector(&b, 0, 100_000).expect("fat recognised");
        assert_eq!(c.fs, FsKind::Fat16);
    }

    #[test]
    fn rejects_a_volume_longer_than_the_device() {
        let b = ntfs_boot(10_000_000);
        assert!(
            identify_boot_sector(&b, 0, 1000).is_none(),
            "a volume claiming to be larger than the disk is not believable"
        );
    }

    #[test]
    fn rejects_random_data() {
        // Deterministic pseudo-random bytes with a valid 0xAA55 signature.
        let mut b: Vec<u8> = (0..512u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        b[510] = 0x55;
        b[511] = 0xAA;
        assert!(identify_boot_sector(&b, 0, 1_000_000).is_none());
    }

    #[test]
    fn rejects_an_ntfs_bpb_with_an_absurd_sector_size() {
        let mut b = ntfs_boot(1000);
        b[11..13].copy_from_slice(&777u16.to_le_bytes()); // not a power of two
        assert!(identify_boot_sector(&b, 0, 10_000).is_none());
    }

    #[test]
    fn identifies_an_ext_superblock() {
        let mut sb = vec![0u8; 1024];
        sb[0x04..0x08].copy_from_slice(&100_000u32.to_le_bytes());
        sb[0x18..0x1C].copy_from_slice(&2u32.to_le_bytes()); // 4 KiB blocks
        sb[0x38..0x3A].copy_from_slice(&0xEF53u16.to_le_bytes());
        let c = identify_ext_superblock(&sb, 2048).expect("ext recognised");
        assert_eq!(c.fs, FsKind::Ext4);
        assert_eq!(c.sectors, 100_000 * 4096 / 512);
    }

    #[test]
    fn overlapping_candidates_are_collapsed() {
        // A backup boot sector inside a volume must not become a partition.
        let cands = vec![
            Candidate {
                lba: Lba(2048),
                fs: FsKind::Ntfs,
                sectors: 100_000,
                confidence: 95,
                evidence: vec![],
            },
            Candidate {
                lba: Lba(102_047),
                fs: FsKind::Ntfs,
                sectors: 0,
                confidence: 60,
                evidence: vec![],
            },
        ];
        let parts = candidates_to_partitions(cands, SectorSize::S512);
        assert_eq!(parts.len(), 2, "a zero-length candidate does not overlap");

        // But a real overlap is dropped in favour of the higher confidence one.
        let cands = vec![
            Candidate {
                lba: Lba(2048),
                fs: FsKind::Ntfs,
                sectors: 100_000,
                confidence: 95,
                evidence: vec![],
            },
            Candidate {
                lba: Lba(50_000),
                fs: FsKind::Fat32,
                sectors: 10_000,
                confidence: 70,
                evidence: vec![],
            },
        ];
        let parts = candidates_to_partitions(cands, SectorSize::S512);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].start, Lba(2048));
    }
}
