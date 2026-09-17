//! APFS and HFS+: detection and header parsing only (SPEC.md 5.4, Milestone 6
//! best-effort).
//!
//! # What is and is not implemented
//!
//! Both filesystems are *recognised* and their headers read, so the CLI can say
//! what the volume is - block size, size, and for APFS the checkpoint the
//! container is at - and then say plainly that deleted-file recovery is not
//! implemented and that carving (`rc carve`) is the way to recover from it.
//!
//! Not implemented: the HFS+ catalog B-tree walk, the APFS object map, older
//! checkpoints and snapshots. Nothing here enumerates a single file.
//!
//! # Why it stopped there
//!
//! No foreign implementation can produce test volumes on this machine: macOS is
//! not available, the WSL kernel has no hfsplus module, and `mkfs.apfs` /
//! `mkfs.hfsplus` can only make empty volumes anyway. A parser for deleted
//! catalog records written without a single real volume to check it against
//! would be exactly the kind of untested feature SPEC.md 9 rules out. The
//! header parsers below are checked against the field layouts in Apple's
//! published specifications (TN1150 for HFS+, the Apple File System Reference
//! for APFS) using hand-built headers, which is weaker evidence than a real
//! volume and is recorded as such in LIMITATIONS.md.
//!
//! On Apple Silicon and T2 Macs the internal APFS volume is encrypted with a
//! hardware key and nothing here or anywhere else can recover it (SPEC.md
//! 1.1).

use crate::error::{FsError, Result};
use rc_device::ReadOnlyDevice;

/// The fields of an APFS container superblock (`nx_superblock_t`) worth
/// showing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApfsContainer {
    pub block_size: u32,
    pub block_count: u64,
    /// Transaction id of this checkpoint.
    pub xid: u64,
    /// Checkpoint descriptor area: first block and block count.
    pub xp_desc_base: u64,
    pub xp_desc_blocks: u32,
    /// Volumes with a non-zero object id in `nx_fs_oid`.
    pub volumes: usize,
}

/// The fields of an HFS+ / HFSX volume header worth showing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HfsPlusVolume {
    /// `H+` or `HX`.
    pub signature: [u8; 2],
    pub version: u16,
    pub block_size: u32,
    pub total_blocks: u32,
    pub free_blocks: u32,
    pub file_count: u32,
    pub folder_count: u32,
    /// The volume was not unmounted cleanly (attribute bit 8 clear).
    pub unmounted_uncleanly: bool,
    /// Journaled (attribute bit 13).
    pub journaled: bool,
}

pub fn is_apfs(block0: &[u8]) -> bool {
    block0.len() >= 36 && &block0[32..36] == b"NXSB"
}

pub fn is_hfsplus(header: &[u8]) -> bool {
    header.len() >= 2 && (&header[..2] == b"H+" || &header[..2] == b"HX")
}

pub fn parse_apfs(b: &[u8]) -> Result<ApfsContainer> {
    if !is_apfs(b) || b.len() < 0xB8 + 100 * 8 {
        return Err(FsError::BadBootSector {
            detail: "no APFS container superblock (NXSB at +32)".into(),
        });
    }
    let u32at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let u64at = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let block_size = u32at(36);
    if !(4096..=65536).contains(&block_size) || !block_size.is_power_of_two() {
        return Err(FsError::BadBootSector {
            detail: format!("APFS block size {block_size} is out of range"),
        });
    }
    let max_fs = u32at(0xB4).min(100) as usize;
    Ok(ApfsContainer {
        block_size,
        block_count: u64at(40),
        xid: u64at(16),
        xp_desc_base: u64at(0x70),
        xp_desc_blocks: u32at(0x68) & 0x7FFF_FFFF,
        volumes: (0..max_fs).filter(|i| u64at(0xB8 + 8 * i) != 0).count(),
    })
}

/// `h` starts at the volume header, 1024 bytes into the volume.
pub fn parse_hfsplus(h: &[u8]) -> Result<HfsPlusVolume> {
    if !is_hfsplus(h) || h.len() < 512 {
        return Err(FsError::BadBootSector {
            detail: "no HFS+ volume header (H+ or HX at +1024)".into(),
        });
    }
    let u32at = |o: usize| u32::from_be_bytes(h[o..o + 4].try_into().unwrap());
    let attributes = u32at(4);
    let block_size = u32at(40);
    if block_size < 512 || !block_size.is_power_of_two() {
        return Err(FsError::BadBootSector {
            detail: format!("HFS+ block size {block_size} is not a power of two >= 512"),
        });
    }
    Ok(HfsPlusVolume {
        signature: [h[0], h[1]],
        version: u16::from_be_bytes([h[2], h[3]]),
        block_size,
        total_blocks: u32at(44),
        free_blocks: u32at(48),
        file_count: u32at(32),
        folder_count: u32at(36),
        unmounted_uncleanly: attributes & (1 << 8) == 0,
        journaled: attributes & (1 << 13) != 0,
    })
}

/// Read and describe the volume at `base`, for the refusal message.
pub fn describe(device: &dyn ReadOnlyDevice, base: u64) -> Result<Option<String>> {
    let mut b = vec![0u8; 4096];
    device.read_bytes_at(base, &mut b)?;
    if is_apfs(&b) {
        let c = parse_apfs(&b)?;
        return Ok(Some(format!(
            "APFS container: {} blocks of {} bytes, checkpoint xid {}, {} volume(s)",
            c.block_count, c.block_size, c.xid, c.volumes
        )));
    }
    if is_hfsplus(&b[1024..]) {
        let v = parse_hfsplus(&b[1024..1536])?;
        return Ok(Some(format!(
            "{} volume: {} blocks of {} bytes ({} free), {} files, {} folders{}{}",
            if &v.signature == b"HX" {
                "HFSX"
            } else {
                "HFS+"
            },
            v.total_blocks,
            v.block_size,
            v.free_blocks,
            v.file_count,
            v.folder_count,
            if v.journaled { ", journaled" } else { "" },
            if v.unmounted_uncleanly {
                ", not cleanly unmounted"
            } else {
                ""
            },
        )));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apfs_container_fields_are_at_their_documented_offsets() {
        let mut b = vec![0u8; 4096];
        b[16..24].copy_from_slice(&77u64.to_le_bytes()); // o_xid
        b[32..36].copy_from_slice(b"NXSB");
        b[36..40].copy_from_slice(&4096u32.to_le_bytes()); // nx_block_size
        b[40..48].copy_from_slice(&262_144u64.to_le_bytes()); // nx_block_count
        b[0x68..0x6C].copy_from_slice(&8u32.to_le_bytes()); // nx_xp_desc_blocks
        b[0x70..0x78].copy_from_slice(&1u64.to_le_bytes()); // nx_xp_desc_base
        b[0xB4..0xB8].copy_from_slice(&100u32.to_le_bytes()); // nx_max_file_systems
        b[0xB8..0xC0].copy_from_slice(&1026u64.to_le_bytes()); // nx_fs_oid[0]
        let c = parse_apfs(&b).unwrap();
        assert_eq!(
            c,
            ApfsContainer {
                block_size: 4096,
                block_count: 262_144,
                xid: 77,
                xp_desc_base: 1,
                xp_desc_blocks: 8,
                volumes: 1
            }
        );
    }

    #[test]
    fn hfsplus_header_fields_are_big_endian_at_their_documented_offsets() {
        let mut h = vec![0u8; 512];
        h[0..2].copy_from_slice(b"H+");
        h[2..4].copy_from_slice(&4u16.to_be_bytes());
        h[4..8].copy_from_slice(&((1u32 << 8) | (1 << 13)).to_be_bytes());
        h[32..36].copy_from_slice(&12u32.to_be_bytes());
        h[36..40].copy_from_slice(&3u32.to_be_bytes());
        h[40..44].copy_from_slice(&4096u32.to_be_bytes());
        h[44..48].copy_from_slice(&1000u32.to_be_bytes());
        h[48..52].copy_from_slice(&400u32.to_be_bytes());
        let v = parse_hfsplus(&h).unwrap();
        assert_eq!(v.block_size, 4096);
        assert_eq!((v.total_blocks, v.free_blocks), (1000, 400));
        assert_eq!((v.file_count, v.folder_count), (12, 3));
        assert!(v.journaled && !v.unmounted_uncleanly);
    }

    #[test]
    fn garbage_is_not_apfs_or_hfsplus() {
        assert!(parse_apfs(&[0u8; 4096]).is_err());
        assert!(parse_hfsplus(&[0u8; 512]).is_err());
        let mut b = vec![0u8; 4096];
        b[32..36].copy_from_slice(b"NXSB");
        b[36..40].copy_from_slice(&1000u32.to_le_bytes());
        assert!(parse_apfs(&b).is_err(), "a 1000-byte block size is invalid");
    }
}
