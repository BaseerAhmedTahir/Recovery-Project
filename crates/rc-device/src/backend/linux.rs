//! Linux raw block device backend.
//!
//! Per SPEC.md section 5.1: `open(O_RDONLY | O_DIRECT)` on `/dev/sdX` and
//! `/dev/nvmeXn1`, falling back to buffered reads plus
//! `posix_fadvise(POSIX_FADV_DONTNEED)` when O_DIRECT alignment is refused.
//!
//! The fallback matters on stacked devices (device-mapper, LUKS, some loop
//! configurations) that reject O_DIRECT outright. Dropping the page cache after
//! each read keeps a whole-disk scan from evicting everything else on the
//! machine, which is the reason O_DIRECT was wanted in the first place.

use crate::error::{DeviceError, Result};
use crate::geometry::{DeviceId, DeviceInfo, DeviceKind, Lba, SectorSize, TrimSupport};
use crate::readonly::{sealed::Sealed, validate_read, ReadOnlyDevice};
use crate::registry::ScanSourceGuard;

use std::fs;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// How the device was opened, which determines post-read cache handling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Direct,
    BufferedFadvise,
}

pub struct LinuxDevice {
    file: fs::File,
    info: DeviceInfo,
    mode: Mode,
    _guard: ScanSourceGuard,
}

fn sysfs_str(dev_name: &str, attr: &str) -> Option<String> {
    let p = format!("/sys/block/{dev_name}/{attr}");
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// Map `/dev/sda1` to its parent `sda`, and `/dev/nvme0n1p2` to `nvme0n1`,
/// because the sysfs attributes we want live on the parent block device.
fn base_block_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_string();
    if fs::metadata(format!("/sys/block/{name}")).is_ok() {
        return Some(name);
    }
    // Partition: strip the trailing partition suffix.
    let trimmed = if let Some(idx) = name.rfind('p') {
        if name[idx + 1..].chars().all(|c| c.is_ascii_digit()) && idx > 0 {
            &name[..idx]
        } else {
            name.trim_end_matches(|c: char| c.is_ascii_digit())
        }
    } else {
        name.trim_end_matches(|c: char| c.is_ascii_digit())
    };
    if fs::metadata(format!("/sys/block/{trimmed}")).is_ok() {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn probe(path: &Path) -> Result<DeviceInfo> {
    let meta = fs::metadata(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => DeviceError::NotFound(path.to_path_buf()),
        _ => DeviceError::Io(e),
    })?;

    let base = base_block_name(path);

    let logical = base
        .as_deref()
        .and_then(|b| sysfs_str(b, "queue/logical_block_size"))
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(512);
    let sector_size = SectorSize::new(logical)?;

    let physical = base
        .as_deref()
        .and_then(|b| sysfs_str(b, "queue/physical_block_size"))
        .and_then(|s| s.parse::<u32>().ok())
        .and_then(|v| SectorSize::new(v).ok());

    // `size` in sysfs is always in 512-byte units regardless of block size.
    let total_bytes = if let Some(sz) = base
        .as_deref()
        .and_then(|b| sysfs_str(b, "size"))
        .and_then(|s| s.parse::<u64>().ok())
    {
        sz * 512
    } else {
        block_size_via_ioctl(path).unwrap_or(meta.len())
    };

    let rotational = base
        .as_deref()
        .and_then(|b| sysfs_str(b, "queue/rotational"))
        .map(|s| s == "1");

    // A non-zero discard granularity means the device advertises TRIM/UNMAP.
    let trim = match base
        .as_deref()
        .and_then(|b| sysfs_str(b, "queue/discard_granularity"))
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(0) => TrimSupport::No,
        Some(_) => TrimSupport::Yes,
        None => TrimSupport::Unknown,
    };

    let serial = base
        .as_deref()
        .and_then(|b| sysfs_str(b, "device/serial"))
        .filter(|s| !s.is_empty());
    let model = base
        .as_deref()
        .and_then(|b| sysfs_str(b, "device/model"))
        .filter(|s| !s.is_empty());
    let removable = base
        .as_deref()
        .and_then(|b| sysfs_str(b, "removable"))
        .map(|s| s == "1");

    let canonical = fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string();
    let id = match &serial {
        Some(s) => DeviceId::Hardware {
            serial: s.clone(),
            total_bytes,
        },
        None => DeviceId::Path(canonical),
    };

    Ok(DeviceInfo {
        id,
        path: path.to_path_buf(),
        kind: if base.as_deref() == path.file_name().map(|n| n.to_string_lossy()).as_deref() {
            DeviceKind::PhysicalDisk
        } else {
            DeviceKind::Volume
        },
        model,
        serial,
        sector_size,
        physical_sector_size: physical,
        total_sectors: total_bytes / sector_size.get() as u64,
        rotational,
        trim,
        removable,
        readable: true,
        access_note: None,
    })
}

/// `BLKGETSIZE64` for devices that do not expose a usable sysfs `size`.
fn block_size_via_ioctl(path: &Path) -> Option<u64> {
    const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: c is a valid NUL-terminated path.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mut size: u64 = 0;
    // SAFETY: fd is open; size is a valid out-pointer of the expected width.
    let rc = unsafe { libc::ioctl(fd, BLKGETSIZE64, &mut size as *mut u64) };
    // SAFETY: fd was opened above and is not used again.
    unsafe { libc::close(fd) };
    if rc == 0 && size > 0 {
        Some(size)
    } else {
        None
    }
}

fn open_fd(path: &Path, direct: bool) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut flags = libc::O_RDONLY | libc::O_CLOEXEC;
    if direct {
        flags |= libc::O_DIRECT;
    }
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
}

impl LinuxDevice {
    pub fn open(path: &Path) -> Result<Self> {
        let info = probe(path)?;

        let (file, mode) = match open_fd(path, true) {
            Ok(f) => (f, Mode::Direct),
            Err(e)
                // ENOTSUP and EOPNOTSUPP are the same value on Linux, so
                // matching both would be an unreachable pattern.
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EINVAL) | Some(libc::ENOTSUP)
                ) =>
            {
                tracing::warn!(
                    path = %path.display(),
                    "O_DIRECT refused; falling back to buffered reads with POSIX_FADV_DONTNEED"
                );
                (open_fd(path, false)?, Mode::BufferedFadvise)
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(DeviceError::NeedsElevation {
                    path: path.to_path_buf(),
                    hint: "run as root, or add your user to the 'disk' group",
                })
            }
            Err(e) => return Err(DeviceError::Io(e)),
        };

        let guard = ScanSourceGuard::register(info.id.clone(), &info.path);
        Ok(LinuxDevice {
            file,
            info,
            mode,
            _guard: guard,
        })
    }

    /// Drop the just-read range from the page cache so a full-disk scan does
    /// not evict the rest of the system's working set.
    fn drop_cache(fd: RawFd, offset: u64, len: usize) {
        // SAFETY: fd is open for the lifetime of self; the call only advises.
        unsafe {
            libc::posix_fadvise(
                fd,
                offset as libc::off_t,
                len as libc::off_t,
                libc::POSIX_FADV_DONTNEED,
            );
        }
    }
}

impl Sealed for LinuxDevice {}

impl ReadOnlyDevice for LinuxDevice {
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

        if self.mode == Mode::BufferedFadvise {
            Self::drop_cache(self.file.as_raw_fd(), offset, done);
        }
        Ok(done)
    }
}

/// Enumerate whole block devices from `/sys/block`, skipping virtual ones
/// (loop, ram, zram, device-mapper) unless they are backed by a real image.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    let dir = match fs::read_dir("/sys/block") {
        Ok(d) => d,
        Err(e) => return Err(DeviceError::Enumerate(e.to_string())),
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("ram") || name.starts_with("zram") || name.starts_with("dm-") {
            continue;
        }
        let path = PathBuf::from(format!("/dev/{name}"));
        if !path.exists() {
            continue;
        }
        match probe(&path) {
            Ok(mut info) => {
                // A zero-length block device is an unbacked loop device. It is
                // not a drive the operator can scan, and listing a dozen of
                // them buries the real disks.
                if info.total_sectors == 0 {
                    continue;
                }
                // Report readability honestly without holding the handle open.
                match open_fd(&path, false) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        info.readable = false;
                        info.access_note = Some("requires root to read sector data".to_string());
                    }
                    Err(e) => {
                        info.readable = false;
                        info.access_note = Some(e.to_string());
                    }
                }
                out.push(info);
            }
            Err(e) => tracing::debug!(path = %path.display(), %e, "skipping device"),
        }
    }
    Ok(out)
}

pub fn is_elevated() -> bool {
    // SAFETY: geteuid is always safe to call.
    unsafe { libc::geteuid() == 0 }
}
