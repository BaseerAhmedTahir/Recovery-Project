//! Filesystem detection and the single entry point for scanning a volume.
//!
//! Detection reads the boot sector rather than trusting a partition type code
//! or GPT type GUID, because those are routinely wrong: MBR type 0x07 is used
//! for NTFS, exFAT and HPFS alike, and "Microsoft Basic Data" covers all three
//! plus FAT.

use crate::entry::ScanResult;
use crate::error::{FsError, Result};
use rc_device::ReadOnlyDevice;
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsType {
    Ntfs,
    Fat12,
    Fat16,
    Fat32,
    ExFat,
    /// ext2, ext3 and ext4, with JBD2 journal recovery.
    Ext4,
    /// Recognised and its container superblock read; no file enumeration.
    Apfs,
    /// Recognised and its volume header read; no file enumeration.
    HfsPlus,
    Unknown,
}

impl FsType {
    pub fn is_supported(self) -> bool {
        matches!(
            self,
            FsType::Ntfs
                | FsType::Fat12
                | FsType::Fat16
                | FsType::Fat32
                | FsType::ExFat
                | FsType::Ext4
        )
    }
}

impl fmt::Display for FsType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FsType::Ntfs => "ntfs",
            FsType::Fat12 => "fat12",
            FsType::Fat16 => "fat16",
            FsType::Fat32 => "fat32",
            FsType::ExFat => "exfat",
            FsType::Ext4 => "ext4",
            FsType::Apfs => "apfs",
            FsType::HfsPlus => "hfs+",
            FsType::Unknown => "unknown",
        })
    }
}

/// Identify the filesystem at `base` bytes into `device`.
pub fn detect(device: &dyn ReadOnlyDevice, base: u64) -> Result<FsType> {
    let mut sector = vec![0u8; 512.max(device.sector_size().as_usize())];
    device.read_bytes_at(base, &mut sector)?;

    // Order matters: exFAT and NTFS both carry an OEM id in the same place, so
    // check the exact strings before falling back to the FAT BPB heuristics.
    if sector.len() >= 11 {
        if &sector[3..11] == b"NTFS    " {
            return Ok(FsType::Ntfs);
        }
        if &sector[3..11] == b"EXFAT   " {
            return Ok(FsType::ExFat);
        }
    }

    if let Ok(b) = crate::fat::bpb::parse_bpb(&sector) {
        return Ok(match b.kind {
            crate::fat::bpb::FatKind::Fat12 => FsType::Fat12,
            crate::fat::bpb::FatKind::Fat16 => FsType::Fat16,
            crate::fat::bpb::FatKind::Fat32 => FsType::Fat32,
        });
    }

    // ext superblock sits 1024 bytes into the volume.
    let mut sb = vec![0u8; 1024];
    if device.read_bytes_at(base + 1024, &mut sb).is_ok()
        && sb.len() >= 0x3A
        && u16::from_le_bytes([sb[0x38], sb[0x39]]) == 0xEF53
    {
        return Ok(FsType::Ext4);
    }

    // APFS: NXSB at +32 of the container. HFS+: H+ or HX at +1024.
    if crate::apple::is_apfs(&sector) {
        return Ok(FsType::Apfs);
    }
    if crate::apple::is_hfsplus(&sb) {
        return Ok(FsType::HfsPlus);
    }

    Ok(FsType::Unknown)
}

/// Detect and scan in one step.
///
/// Unimplemented filesystems return [`FsError::NotImplemented`] rather than an
/// empty result, so the CLI can say "APFS is not supported" instead of
/// "no deleted files found" - which would be a lie (SPEC.md section 9: no
/// stubs pretending to be features).
pub fn scan_volume(device: &dyn ReadOnlyDevice, base: u64) -> Result<(FsType, ScanResult)> {
    scan_volume_with(device, base, &mut crate::ScanCtx::quiet())
}

/// As [`scan_volume`], reporting progress and able to stop part-way. A scan
/// that stopped early says so in its notes and returns what it had.
pub fn scan_volume_with(
    device: &dyn ReadOnlyDevice,
    base: u64,
    ctx: &mut crate::ScanCtx,
) -> Result<(FsType, ScanResult)> {
    let fs = detect(device, base)?;
    let result = match fs {
        FsType::Ntfs => crate::ntfs::NtfsVolume::open(device, base)?.scan_with(ctx)?,
        FsType::Fat12 | FsType::Fat16 | FsType::Fat32 => {
            crate::fat::FatVolume::open(device, base)?.scan()?
        }
        FsType::ExFat => crate::exfat::ExfatVolume::open(device, base)?.scan()?,
        FsType::Ext4 => crate::ext4::Ext4Volume::open(device, base)?.scan()?,
        FsType::Apfs | FsType::HfsPlus => {
            let what = crate::apple::describe(device, base)?.unwrap_or_default();
            return Err(FsError::NotImplemented {
                fs: if fs == FsType::Apfs { "APFS" } else { "HFS+" },
                detail: format!(
                    "{what}. Deleted-file recovery from this filesystem is not implemented (header parsing only); use `rc carve` to recover files by signature."
                ),
            });
        }
        FsType::Unknown => {
            return Err(FsError::Unrecognised {
                detail: format!("no known filesystem signature at byte offset {base}"),
            })
        }
    };
    Ok((fs, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_set_matches_milestone_two() {
        assert!(FsType::Ntfs.is_supported());
        assert!(FsType::Fat32.is_supported());
        assert!(FsType::ExFat.is_supported());
        assert!(FsType::Ext4.is_supported(), "ext4 landed in Milestone 6");
        assert!(!FsType::Apfs.is_supported(), "APFS is detection only");
        assert!(!FsType::HfsPlus.is_supported(), "HFS+ is detection only");
        assert!(!FsType::Unknown.is_supported());
    }

    #[test]
    fn display_names_are_stable_for_the_cli() {
        assert_eq!(FsType::Ntfs.to_string(), "ntfs");
        assert_eq!(FsType::ExFat.to_string(), "exfat");
        assert_eq!(FsType::Fat32.to_string(), "fat32");
    }
}
