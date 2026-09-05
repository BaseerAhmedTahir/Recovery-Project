//! Resolve an output path to the physical device that backs it.
//!
//! This implements SPEC.md section 4.2: before writing anything, work out
//! which physical device a destination lands on, so we can hard-fail if it is
//! the device being scanned. Writing recovered files back onto the drive you
//! are recovering from is the single most common way to destroy the very data
//! you are trying to save.
//!
//! Resolution is best-effort by nature - a destination can be a network share,
//! a RAM disk, or a fuse mount with no meaningful physical device. The
//! important property is that it never returns a *wrong* answer that would let
//! a dangerous write through: when the backing device cannot be determined the
//! result is [`Backing::Unknown`], and the caller decides how strict to be.

use std::path::{Path, PathBuf};

/// What physical storage a path resolves to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Backing {
    /// Resolved to a specific device, named the way the platform names it.
    Device {
        /// e.g. `\\.\PhysicalDrive1`, `/dev/sda`, `/dev/disk2`
        device: String,
        /// Volume/partition the path sits on, when known.
        volume: Option<String>,
        /// Hardware serial, when the platform exposes it cheaply.
        serial: Option<String>,
    },
    /// A real location whose backing device we could not determine
    /// (network share, unusual fuse mount, container overlay).
    Unknown { reason: String },
}

impl Backing {
    pub fn device_name(&self) -> Option<&str> {
        match self {
            Backing::Device { device, .. } => Some(device),
            Backing::Unknown { .. } => None,
        }
    }
}

/// Find the nearest existing ancestor of `path`.
///
/// The destination file usually does not exist yet, so resolution has to work
/// from the directory that will contain it.
pub fn existing_ancestor(path: &Path) -> PathBuf {
    let mut p = path;
    loop {
        if p.exists() {
            return p.to_path_buf();
        }
        match p.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => p = parent,
            _ => return PathBuf::from("."),
        }
    }
}

/// Resolve the device backing `path`.
pub fn resolve(path: &Path) -> Backing {
    let anchor = existing_ancestor(path);
    #[cfg(windows)]
    {
        windows_impl::resolve(&anchor)
    }
    #[cfg(target_os = "linux")]
    {
        linux_impl::resolve(&anchor)
    }
    #[cfg(target_os = "macos")]
    {
        macos_impl::resolve(&anchor)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = anchor;
        Backing::Unknown {
            reason: "unsupported platform".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows_impl {
    use super::Backing;
    use std::path::Path;

    /// Map a path to its volume, then that volume to its physical drive.
    ///
    /// `GetVolumePathNameW` gives the mount point, `GetVolumeNameForVolume-
    /// MountPointW` gives the stable `\\?\Volume{GUID}\` name, and
    /// `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` maps that to physical drive
    /// numbers. A spanned or striped volume returns several extents, in which
    /// case every disk it touches is unsafe to write to.
    pub fn resolve(path: &Path) -> Backing {
        match volume_for_path(path) {
            Some(volume) => match disk_numbers_for_volume(&volume) {
                Some(disks) if !disks.is_empty() => Backing::Device {
                    device: disks
                        .iter()
                        .map(|n| format!("\\\\.\\PhysicalDrive{n}"))
                        .collect::<Vec<_>>()
                        .join(","),
                    volume: Some(volume),
                    serial: None,
                },
                _ => Backing::Unknown {
                    reason: format!("volume {volume} exposed no disk extents"),
                },
            },
            None => Backing::Unknown {
                reason: format!("could not determine the volume for {}", path.display()),
            },
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    fn volume_for_path(path: &Path) -> Option<String> {
        use windows_sys::Win32::Storage::FileSystem::{
            GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
        };

        let wpath = wide(&path.to_string_lossy());
        let mut mount = vec![0u16; 512];
        // SAFETY: wpath is NUL-terminated; mount is a live buffer of the given length.
        let ok = unsafe { GetVolumePathNameW(wpath.as_ptr(), mount.as_mut_ptr(), 512) };
        if ok == 0 {
            return None;
        }

        let mut guid = vec![0u16; 512];
        // SAFETY: mount is NUL-terminated by the call above.
        let ok =
            unsafe { GetVolumeNameForVolumeMountPointW(mount.as_ptr(), guid.as_mut_ptr(), 512) };
        if ok == 0 {
            return Some(from_wide(&mount));
        }
        Some(from_wide(&guid))
    }

    fn disk_numbers_for_volume(volume: &str) -> Option<Vec<u32>> {
        use std::ffi::c_void;
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::Ioctl::IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS;
        use windows_sys::Win32::System::IO::DeviceIoControl;

        // The IOCTL wants the volume name without its trailing backslash.
        let trimmed = volume.trim_end_matches('\\');
        let wvol = wide(trimmed);

        // SAFETY: wvol is NUL-terminated; zero access rights avoids needing
        // elevation, which is enough for this query.
        let h = unsafe {
            CreateFileW(
                wvol.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return None;
        }

        // Sized for a generously striped volume; VOLUME_DISK_EXTENTS is a
        // variable-length struct with a trailing array.
        let mut buf = vec![0u8; 4096];
        let mut returned = 0u32;
        // SAFETY: h is a live handle; buf is a live buffer of the given size.
        let ok = unsafe {
            DeviceIoControl(
                h,
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                std::ptr::null(),
                0,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: h was opened above and is not used again.
        unsafe { CloseHandle(h) };
        if ok == 0 || returned < 8 {
            return None;
        }

        // struct VOLUME_DISK_EXTENTS { DWORD NumberOfDiskExtents; DISK_EXTENT Extents[1]; }
        // struct DISK_EXTENT { DWORD DiskNumber; LARGE_INTEGER Start, Length; }
        let count = u32::from_le_bytes(buf[0..4].try_into().ok()?) as usize;
        let mut disks = Vec::with_capacity(count);
        // 8-byte alignment padding after the count before the first extent.
        let mut off = 8usize;
        for _ in 0..count {
            if off + 24 > buf.len() {
                break;
            }
            disks.push(u32::from_le_bytes(buf[off..off + 4].try_into().ok()?));
            off += 24;
        }
        disks.sort_unstable();
        disks.dedup();
        Some(disks)
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::Backing;
    use std::path::Path;

    /// Walk `/proc/self/mountinfo` for the longest mount point that prefixes
    /// the path, then map that mount's source to a whole block device.
    pub fn resolve(path: &Path) -> Backing {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return Backing::Unknown {
                reason: "could not read /proc/self/mountinfo".to_string(),
            };
        };

        let mut best: Option<(usize, String, String)> = None; // (len, mountpoint, source)
        for line in mountinfo.lines() {
            // <id> <parent> <maj:min> <root> <mountpoint> ... - <fstype> <source> <opts>
            let Some((left, right)) = line.split_once(" - ") else {
                continue;
            };
            let lf: Vec<&str> = left.split_whitespace().collect();
            let rf: Vec<&str> = right.split_whitespace().collect();
            if lf.len() < 5 || rf.len() < 2 {
                continue;
            }
            let mountpoint = unescape(lf[4]);
            let source = rf[1].to_string();
            if canonical.starts_with(&mountpoint)
                && best
                    .as_ref()
                    .map_or(true, |(l, _, _)| mountpoint.len() > *l)
            {
                best = Some((mountpoint.len(), mountpoint, source));
            }
        }

        match best {
            Some((_, _mountpoint, source)) if source.starts_with("/dev/") => {
                let whole = whole_disk_for(&source).unwrap_or_else(|| source.clone());
                Backing::Device {
                    device: whole,
                    volume: Some(source),
                    serial: None,
                }
            }
            Some((_, mountpoint, source)) => Backing::Unknown {
                reason: format!("{mountpoint} is backed by {source}, not a block device"),
            },
            None => Backing::Unknown {
                reason: "no matching mount point".to_string(),
            },
        }
    }

    /// `/dev/sda1` -> `/dev/sda`, `/dev/nvme0n1p2` -> `/dev/nvme0n1`.
    fn whole_disk_for(dev: &str) -> Option<String> {
        let name = dev.strip_prefix("/dev/")?;
        if std::fs::metadata(format!("/sys/block/{name}")).is_ok() {
            return Some(dev.to_string());
        }
        for entry in std::fs::read_dir("/sys/block").ok()?.flatten() {
            let parent = entry.file_name().to_string_lossy().to_string();
            if std::fs::metadata(format!("/sys/block/{parent}/{name}")).is_ok() {
                return Some(format!("/dev/{parent}"));
            }
        }
        None
    }

    /// mountinfo escapes space, tab, newline and backslash as octal.
    fn unescape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\' && i + 3 < b.len() {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8) {
                    out.push(v as char);
                    i += 4;
                    continue;
                }
            }
            out.push(b[i] as char);
            i += 1;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos_impl {
    use super::Backing;
    use std::path::Path;

    /// `statfs` gives the mount source directly, e.g. `/dev/disk3s5`.
    pub fn resolve(path: &Path) -> Backing {
        use std::os::unix::ffi::OsStrExt;
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return Backing::Unknown {
                reason: "path contained an interior NUL".to_string(),
            };
        };
        // SAFETY: c is a valid NUL-terminated path; sfs is a valid out-pointer.
        let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(c.as_ptr(), &mut sfs) } != 0 {
            return Backing::Unknown {
                reason: "statfs failed".to_string(),
            };
        }
        let raw: Vec<u8> = sfs
            .f_mntfromname
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        let source = String::from_utf8_lossy(&raw).to_string();
        if !source.starts_with("/dev/") {
            return Backing::Unknown {
                reason: format!("mounted from {source}, not a block device"),
            };
        }
        Backing::Device {
            device: whole_disk_for(&source),
            volume: Some(source),
            serial: None,
        }
    }

    /// `/dev/disk3s5` -> `/dev/disk3`.
    fn whole_disk_for(dev: &str) -> String {
        if let Some(rest) = dev.strip_prefix("/dev/disk") {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                return format!("/dev/disk{digits}");
            }
        }
        dev.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_nearest_existing_ancestor() {
        let tmp = std::env::temp_dir();
        let deep = tmp.join("rc-does-not-exist-a/b/c/out.raw");
        let anchor = existing_ancestor(&deep);
        assert!(
            anchor.exists(),
            "resolved anchor {} should exist",
            anchor.display()
        );
        assert!(deep.starts_with(&anchor));
    }

    #[test]
    fn an_existing_path_anchors_to_itself() {
        let tmp = std::env::temp_dir();
        assert_eq!(existing_ancestor(&tmp), tmp);
    }

    /// The temp directory must resolve to something; whether it names a device
    /// or reports Unknown depends on the platform, but it must not panic.
    #[test]
    fn resolves_the_temp_directory_without_panicking() {
        let b = resolve(&std::env::temp_dir());
        match &b {
            Backing::Device { device, .. } => assert!(!device.is_empty()),
            Backing::Unknown { reason } => assert!(!reason.is_empty()),
        }
    }
}
