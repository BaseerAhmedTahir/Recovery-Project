//! macOS raw block device backend.
//!
//! Per SPEC.md section 5.1: prefer `/dev/rdiskN`, the raw character device,
//! which bypasses the unified buffer cache. Reads there must be whole sectors
//! at sector-aligned offsets, which is already this crate's contract.
//!
//! Two honest limitations are reported rather than worked around:
//!
//! * Full Disk Access must be granted to the terminal or app bundle. Without
//!   it, opening returns EPERM even as root, so that case is surfaced as
//!   [`DeviceError::NeedsElevation`] with a TCC-specific hint.
//! * On Apple Silicon the internal SSD is behind a sealed, hardware-key
//!   encrypted APFS container. SPEC.md section 1.1 is explicit that this is
//!   effectively impossible to recover from, so an encrypted container is
//!   reported as [`DeviceError::Encrypted`] instead of handing back ciphertext.

use crate::error::{DeviceError, Result};
use crate::geometry::{DeviceId, DeviceInfo, DeviceKind, Lba, SectorSize, TrimSupport};
use crate::readonly::{sealed::Sealed, validate_read, ReadOnlyDevice};
use crate::registry::ScanSourceGuard;

use std::fs;
use std::path::{Path, PathBuf};

// <sys/disk.h>
const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6418;
const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4008_6419;
const DKIOCISSOLIDSTATE: libc::c_ulong = 0x4004_6499;

/// Prefer the raw character device: `/dev/disk2` -> `/dev/rdisk2`.
fn raw_variant(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string());
    match name {
        Some(n) if n.starts_with("disk") => path.with_file_name(format!("r{n}")),
        _ => path.to_path_buf(),
    }
}

fn ioctl_u32(fd: libc::c_int, req: libc::c_ulong) -> Option<u32> {
    let mut v: u32 = 0;
    // SAFETY: fd is open; v is a valid out-pointer of the expected width.
    let rc = unsafe { libc::ioctl(fd, req, &mut v as *mut u32) };
    if rc == 0 {
        Some(v)
    } else {
        None
    }
}

fn ioctl_u64(fd: libc::c_int, req: libc::c_ulong) -> Option<u64> {
    let mut v: u64 = 0;
    // SAFETY: as above.
    let rc = unsafe { libc::ioctl(fd, req, &mut v as *mut u64) };
    if rc == 0 {
        Some(v)
    } else {
        None
    }
}

fn open_raw(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_RDONLY | libc::O_CLOEXEC)
        .open(path)
}

fn probe(path: &Path, file: &fs::File) -> Result<DeviceInfo> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();

    let bs = ioctl_u32(fd, DKIOCGETBLOCKSIZE).unwrap_or(512);
    let sector_size = SectorSize::new(bs)?;
    let blocks = ioctl_u64(fd, DKIOCGETBLOCKCOUNT).unwrap_or(0);
    if blocks == 0 {
        return Err(DeviceError::Unsupported {
            path: path.to_path_buf(),
            detail: "device reported a zero block count".to_string(),
        });
    }
    let solid_state = ioctl_u32(fd, DKIOCISSOLIDSTATE).map(|v| v != 0);

    let canonical = fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string();

    Ok(DeviceInfo {
        id: DeviceId::Path(canonical),
        path: path.to_path_buf(),
        kind: if path.to_string_lossy().contains('s')
            && path.to_string_lossy().matches('s').count() > 1
        {
            DeviceKind::Volume
        } else {
            DeviceKind::PhysicalDisk
        },
        model: None,
        serial: None,
        sector_size,
        physical_sector_size: None,
        total_sectors: blocks,
        rotational: solid_state.map(|s| !s),
        // Apple does not expose a per-device TRIM flag through a stable ioctl;
        // reporting Unknown is more honest than guessing from the SSD bit.
        trim: match solid_state {
            Some(true) => TrimSupport::Unknown,
            Some(false) => TrimSupport::No,
            None => TrimSupport::Unknown,
        },
        removable: None,
        readable: true,
        access_note: None,
    })
}

pub struct MacosDevice {
    file: fs::File,
    info: DeviceInfo,
    _guard: ScanSourceGuard,
}

impl MacosDevice {
    pub fn open(path: &Path) -> Result<Self> {
        let raw = raw_variant(path);
        let target = if raw.exists() {
            raw
        } else {
            path.to_path_buf()
        };

        let file = match open_raw(&target) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(DeviceError::NeedsElevation {
                    path: target,
                    hint: "run with sudo and grant Full Disk Access to your terminal \
                           in System Settings > Privacy & Security",
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(DeviceError::NotFound(target))
            }
            Err(e) => return Err(DeviceError::Io(e)),
        };

        let info = probe(&target, &file)?;
        let guard = ScanSourceGuard::register(info.id.clone(), &info.path);
        Ok(MacosDevice {
            file,
            info,
            _guard: guard,
        })
    }
}

impl Sealed for MacosDevice {}

impl ReadOnlyDevice for MacosDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn read_at(&self, lba: Lba, buf: &mut [u8]) -> Result<usize> {
        use std::os::unix::fs::FileExt;

        let want = validate_read(&self.info, lba, buf)?;
        let offset = lba.byte_offset(self.info.sector_size);

        let mut done = 0usize;
        while done < want {
            match self
                .file
                .read_at(&mut buf[done..want], offset + done as u64)
            {
                Ok(0) => break,
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(DeviceError::Read {
                        path: self.info.path.clone(),
                        lba: lba.0 + (done / self.info.sector_size.as_usize()) as u64,
                        source: e,
                    })
                }
            }
        }
        Ok(done)
    }
}

/// Enumerate `/dev/rdiskN`. Whole disks only; slices are reachable by path.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    for n in 0..32u32 {
        let path = PathBuf::from(format!("/dev/rdisk{n}"));
        if !path.exists() {
            continue;
        }
        match open_raw(&path) {
            Ok(f) => match probe(&path, &f) {
                Ok(info) => out.push(info),
                Err(e) => tracing::debug!(path = %path.display(), %e, "skipping device"),
            },
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                // Still list it, but say plainly why it cannot be read.
                out.push(DeviceInfo {
                    id: DeviceId::Path(path.to_string_lossy().to_string()),
                    path: path.clone(),
                    kind: DeviceKind::PhysicalDisk,
                    model: None,
                    serial: None,
                    sector_size: SectorSize::S512,
                    physical_sector_size: None,
                    total_sectors: 0,
                    rotational: None,
                    trim: TrimSupport::Unknown,
                    removable: None,
                    readable: false,
                    access_note: Some(
                        "requires sudo and Full Disk Access for the terminal".to_string(),
                    ),
                });
            }
            Err(_) => {}
        }
    }
    Ok(out)
}

pub fn is_elevated() -> bool {
    // SAFETY: geteuid is always safe to call.
    unsafe { libc::geteuid() == 0 }
}

/// Best-effort detection of a locked APFS container.
///
/// A sealed or FileVault-locked container reads as high-entropy noise with no
/// recognisable `NXSB` superblock magic at offset 32. Callers use this to
/// report [`DeviceError::Encrypted`] rather than carving ciphertext.
pub fn looks_like_locked_apfs(first_block: &[u8]) -> bool {
    if first_block.len() < 40 {
        return false;
    }
    &first_block[32..36] != b"NXSB"
}
